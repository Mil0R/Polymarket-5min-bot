#!/usr/bin/env python3
"""SDDFV-only terminal TWAP distribution calibration and frozen replay.

Targets are final/current log returns in bps, not bps/sqrt(second).
All predictors are available at the signal; final prices only fit/score targets.
No orders, network requests, or changes to the winner-trading estimator.
"""
import argparse
import collections
import datetime as dt
import hashlib
import json
import math
from pathlib import Path

import digital_fair_value_residual_study as execution
import digital_fair_value_risk as risk
import digital_fair_value_observation_audit as capture_audit

FEATURES = ["twap_change_30_bps", "twap_change_60_bps", "chainlink_basis_bps", "reference_age_secs", "effective_horizon_secs"]
POLICY = dict(execution.POLICY, min_train=100, min_validation=50, min_holdout=50,
              min_horizon_checkpoint_ratio=0.75, max_opening_error_bps=0.5)


def number(value):
    return execution.finite(value)


def log_bps(a, b):
    a, b = number(a), number(b)
    return math.log(a / b) * 10000 if a and b and a > 0 and b > 0 else None


def signal_features(signal, end_ms):
    now = number(signal.get("sampled_at_ms"))
    audit = signal.get("model_audit", {})
    observed = number(audit.get("reference_observed_at_ms"))
    if observed is None:
        observed = number(signal.get("reference_timing", {}).get("observed_at_ms"))
    margin = log_bps(signal.get("current_twap"), signal.get("opening_twap"))
    if now is None or observed is None or margin is None or end_ms <= now or not 0 <= now-observed <= 5000:
        raise ValueError("invalid_signal_reference_or_horizon")
    remaining = (end_ms-now)/1000
    logged_remaining = number(signal.get("seconds_remaining"))
    if logged_remaining is None or abs(logged_remaining-remaining) > 0.05:
        raise ValueError("signal_remaining_time_mismatch")
    result = dict(margin_bps=margin, horizon_secs=(end_ms-observed)/1000,
                  reference_age_secs=(now-observed)/1000, vector=None,
                  conditional_unavailable="missing_twap_anchors_or_chainlink_spot")
    anchors = {a.get("ago_ms"): a for a in signal.get("twap_anchors", {}).get("anchors", [])}
    changes = []
    for seconds in (30, 60):
        anchor = anchors.get(seconds*1000, {})
        stamp = number(anchor.get("observed_at_ms"))
        change = log_bps(signal.get("current_twap"), anchor.get("value"))
        if stamp is None or change is None:
            return result
        elapsed = (observed-stamp)/1000
        if not seconds-10 <= elapsed <= seconds+10:
            result["conditional_unavailable"] = "anchor_time_outside_support"
            return result
        changes.append(max(-50.0, min(50.0, change*seconds/elapsed)))
    spot = next((s.get("sample") for s in signal.get("spot_asof", {}).get("streams", [])
                 if s.get("topic") == "crypto_prices_chainlink"), None)
    if not isinstance(spot, dict):
        return result
    spot_at, received = number(spot.get("observed_at_ms")), number(spot.get("received_at_ms"))
    basis = log_bps(spot.get("value"), signal.get("current_twap"))
    if (spot_at is None or received is None or basis is None
            or not 0 <= now-spot_at <= 5000 or not 0 <= now-received <= 2000):
        result["conditional_unavailable"] = "spot_not_available_asof_signal"
        return result
    result["vector"] = changes + [max(-50.0, min(50.0, basis)), result["reference_age_secs"], result["horizon_secs"]]
    result["conditional_unavailable"] = None
    return result


def percentile(values, q):
    values = sorted(values)
    pos = q*(len(values)-1)
    lo = int(pos)
    return values[lo] + (values[min(lo+1, len(values)-1)]-values[lo])*(pos-lo)


def solve(matrix, values):
    a = [list(row)+[value] for row, value in zip(matrix, values)]
    for i in range(len(values)):
        pivot = max(range(i, len(values)), key=lambda j: abs(a[j][i]))
        a[i], a[pivot] = a[pivot], a[i]
        if abs(a[i][i]) < 1e-12:
            raise ValueError("singular_training_fit")
        scale = a[i][i]
        a[i] = [x/scale for x in a[i]]
        for j in range(len(values)):
            if j != i:
                scale = a[j][i]
                a[j] = [x-scale*y for x, y in zip(a[j], a[i])]
    return [row[-1] for row in a]


def design(vector, model):
    return [1.0] + [(x-m)/s for x, m, s in zip(vector, model["means"], model["scales"])]


def location(row, model):
    if model["kind"] == "asset_gaussian":
        return model["mean_bps"]
    vector = row["features"]["vector"]
    return sum(a*b for a, b in zip(model["beta"], design(vector, model)))


def activity(row):
    v = row["features"]["vector"]
    return abs(v[0])+abs(v[2])


def fit(rows, kind, ridge=10.0):
    used = rows if kind == "asset_gaussian" else [r for r in rows if r["features"]["vector"] is not None]
    if len(used) < POLICY["min_train"]:
        return None
    horizons = [r["features"]["horizon_secs"] for r in used]
    model = dict(kind=kind, training_samples=len(used), horizon_min_secs=min(horizons)-1,
                 horizon_max_secs=max(horizons)+1, feature_names=FEATURES,
                 reference_topic="crypto_prices_twap_sixty", scale_unit="terminal_return_bps")
    if kind == "asset_gaussian":
        model["mean_bps"] = sum(r["target_bps"] for r in used)/len(used)
    else:
        vectors = [r["features"]["vector"] for r in used]
        model["means"] = [sum(x[j] for x in vectors)/len(vectors) for j in range(len(FEATURES))]
        model["scales"] = [max(0.01, math.sqrt(sum((x[j]-model["means"][j])**2 for x in vectors)/len(vectors)))
                           for j in range(len(FEATURES))]
        xs = [design(x, model) for x in vectors]
        n = len(FEATURES)+1
        matrix = [[sum(x[i]*x[j] for x in xs)+(ridge if i == j and i != 0 else 0)
                   for j in range(n)] for i in range(n)]
        values = [sum(x[i]*r["target_bps"] for x, r in zip(xs, used)) for i in range(n)]
        model.update(beta=solve(matrix, values), ridge=ridge)
    residuals = [r["target_bps"]-location(r, model) for r in used]
    degrees = 1 if kind == "asset_gaussian" else len(FEATURES)+1
    variance = sum(e*e for e in residuals)/max(1, len(residuals)-degrees)
    model["pooled_sigma_bps"] = max(1e-6, math.sqrt(variance))
    # Training-only scale guard. It does not force a constant per-sqrt-second sigma.
    model["sigma_floor_bps"] = max(1e-6, percentile([abs(e) for e in residuals], 0.25))
    model["activity_cutoff_bps"] = None
    model["horizon_cutoff_secs"] = None
    model["regime_sigma_bps"] = [model["pooled_sigma_bps"]]*4
    if kind != "asset_gaussian":
        cutoff = percentile([activity(r) for r in used], 0.5)
        model["activity_cutoff_bps"] = cutoff
        model["horizon_cutoff_secs"] = percentile(horizons, 0.5)
        for regime in range(4):
            errors = [e for r, e in zip(used, residuals) if regime_index(r,model) == regime]
            # Small regimes shrink to the asset estimate rather than to a zero variance.
            if len(errors) >= 40:
                model["regime_sigma_bps"][regime] = math.sqrt((sum(e*e for e in errors)+50*variance)/(len(errors)+50))
    model["standardized_residuals"] = sorted(e/scale(r, model) for r, e in zip(used, residuals))
    return model


def regime_index(row, model):
    if model["activity_cutoff_bps"] is None:
        return 0
    return int(activity(row)>model["activity_cutoff_bps"])+2*int(row["features"]["horizon_secs"]>model["horizon_cutoff_secs"])


def scale(row, model):
    regime = regime_index(row, model)
    return max(model["sigma_floor_bps"], model["regime_sigma_bps"][regime], 1e-6)


def predict(row, model):
    if model["kind"] == "market_offset_logistic":
        vector = offset_features(row, model["terminal_source"])
        if vector is None:
            return None
        z = execution.logit(row["market"]) + sum(b*(x-m)/s for b,x,m,s in
            zip(model["beta"],vector,model["means"],model["scales"]))
        p = execution.sigmoid(z)
        return max(1e-6,min(1-1e-6,max(row["market"]-model["max_correction"],
                   min(row["market"]+model["max_correction"],p))))
    f = row["features"]
    if not model["horizon_min_secs"] <= f["horizon_secs"] <= model["horizon_max_secs"]:
        return None
    if model["kind"] != "asset_gaussian" and f["vector"] is None:
        return None
    mu, sigma = location(row, model), scale(row, model)
    z = (f["margin_bps"]+mu)/sigma
    if model["kind"] == "conditional_empirical":
        threshold = -z
        residuals = model["standardized_residuals"]
        p = (1+sum(r >= threshold for r in residuals))/(len(residuals)+2)
    else:
        p = 0.5*(1+math.erf(z/math.sqrt(2)))
    return max(1e-6, min(1-1e-6, p))


def offset_features(row, source):
    market = row.get("market")
    terminal = predict(row, source)
    if market is None or terminal is None:
        return None
    f = row["features"]
    # Frozen, causal boundary features. No chosen side, delayed book or label.
    return [1.0, terminal-market, max(-10.0,min(10.0,f["margin_bps"]/scale(row,source))),
            max(0.0,min(10.0,abs(f["margin_bps"])/scale(row,source))), f["reference_age_secs"]/5.0]


def fit_offset(rows, source, ridge):
    used = [(r,offset_features(r,source)) for r in rows]
    used = [(r,x) for r,x in used if x is not None]
    if len(used) < POLICY["min_train"]:
        return None
    n = 5
    means = [0.0]+[sum(x[j] for r,x in used)/len(used) for j in range(1,n)]
    scales = [1.0]+[max(.01,math.sqrt(sum((x[j]-means[j])**2 for r,x in used)/len(used))) for j in range(1,n)]
    xs = [[(x-m)/s for x,m,s in zip(x,means,scales)] for r,x in used]
    beta = [0.0]*n
    for _ in range(40):
        ps = [execution.sigmoid(execution.logit(r["market"])+sum(b*x for b,x in zip(beta,z)))
              for (r,_),z in zip(used,xs)]
        grad = [sum((p-r["won"])*z[j] for ((r,_),z,p) in zip(used,xs,ps))+ridge*beta[j] for j in range(n)]
        hess = [[sum(max(1e-6,p*(1-p))*z[j]*z[k] for z,p in zip(xs,ps))+(ridge if j==k else 0)
                 for k in range(n)] for j in range(n)]
        step = solve(hess,grad)
        beta = [b-max(-1.0,min(1.0,s)) for b,s in zip(beta,step)]
        if max(abs(s) for s in step)<1e-9:
            break
    result = dict(source,kind="market_offset_logistic", terminal_source=source, training_samples=len(used),
                  means=means,scales=scales,beta=beta,ridge=ridge,max_correction=.10,
                  offset_feature_names=["intercept","terminal_minus_market","signed_boundary_sigma",
                                        "absolute_boundary_sigma","reference_age_fraction"])
    return result


def replay_with_fee(row, probability, policy):
    p = dict(policy)
    audit = (row.get("episode") or {}).get("signal",{}).get("market_fee_audit",{})
    stamp = number(audit.get("observed_at_ms"))
    now = number((row.get("episode") or {}).get("signal",{}).get("sampled_at_ms"))
    rate, exponent = number(audit.get("rate")), number(audit.get("exponent"))
    verified = (audit.get("verified") is True and rate is not None and 0<=rate<=1
                and exponent in (1,2,3,4) and stamp is not None and now is not None
                and 0<=now-stamp<=10000 and audit.get("slug")==row["slug"])
    if p.get("fee_source")=="observed_required" and not verified:
        return dict(status="excluded",reason="missing_causal_market_fee_schedule")
    if p.get("require_opening_audit"):
        opening=(row.get("episode") or {}).get("signal",{}).get("opening_reference_audit",{})
        stamp=number(opening.get("observed_at_ms"))
        error=log_bps((row.get("episode") or {}).get("signal",{}).get("opening_twap"),opening.get("official_price_to_beat"))
        if stamp is None or now is None or not 0<=now-stamp<=10000 or error is None or abs(error)>p["max_opening_error_bps"]:
            return dict(status="excluded",reason="missing_or_mismatched_causal_official_opening")
        if opening.get("schema_version",1)>=2:
            ep=row["episode"]
            condition=(ep["signal"].get("market_fee_audit") or {}).get("condition_id")
            if (opening.get("verified") is not True or opening.get("verified_before_signal") is not True
                    or opening.get("slug")!=row["slug"] or not condition
                    or opening.get("condition_id")!=condition or opening.get("window_end_ms")!=ep["window_end_ms"]
                    or stamp<ep["window_end_ms"]-300000
                    or opening.get("reason")!="verified_live_opening"):
                return dict(status="excluded",reason="unverified_or_identity_mismatched_live_opening")
            if opening.get("source")=="polymarket_event_page":
                key=opening.get("query_key",[])
                if not isinstance(key,list): key=[]
                updated=number(opening.get("source_data_updated_at_ms"))
                try:
                    start_key=dt.datetime.fromisoformat(key[3].replace("Z","+00:00")).timestamp()*1000
                    end_key=dt.datetime.fromisoformat(key[5].replace("Z","+00:00")).timestamp()*1000
                except (IndexError,AttributeError,ValueError,TypeError):
                    start_key=end_key=None
                if (len(key)!=8 or key[:3]!=["crypto-prices","price",row["asset"].upper()]
                        or key[4]!="fiveminute" or key[6] is not True or type(key[7]) is not int or key[7]!=60
                        or start_key!=ep["window_end_ms"]-300000 or end_key!=ep["window_end_ms"]
                        or updated is None or not ep["window_end_ms"]-300000<=updated<=stamp
                        or opening.get("reference_rule")!="chainlink_twap_sixty"
                        or not isinstance(opening.get("response_sha256"),str) or len(opening["response_sha256"])!=64
                        or any(c not in "0123456789abcdefABCDEF" for c in opening["response_sha256"])):
                    return dict(status="excluded",reason="unverified_page_opening_provenance")
            if opening.get("schema_version",1)>=3 and opening.get("source") not in (
                    "gamma_event_by_slug","gamma_events_list","polymarket_event_page"):
                return dict(status="excluded",reason="unverified_opening_source")
    if verified:
        p.update(fee_rate=rate,fee_exponent=int(exponent))
    result = execution.replay(row,probability,p)
    result["fee_evidence"] = "causal_gamma_schedule" if verified else "historical_static_assumption"
    return result


def load(paths):
    rows, seen, diagnostics, inputs, run_ids = [], {}, collections.Counter(), [], set()
    for name in paths:
        path = Path(name)
        cache_path = path.parent/"reference-audit.json"
        cache = json.loads(cache_path.read_text())
        digest = hashlib.sha256()
        with path.open("rb") as stream:
            for line_no, line in enumerate(stream, 1):
                digest.update(line)
                if not line.strip():
                    continue
                try:
                    e = json.loads(line)
                except ValueError as error:
                    raise ValueError(f"{path}:{line_no}: malformed journal") from error
                if e.get("event") == "run_started":
                    run_ids.add(e["run_id"])
                if e.get("event") != "residual_execution_observation":
                    continue
                slug, checkpoint = e["slug"], int(e["checkpoint_secs"])
                key = (slug, checkpoint)
                if key in seen:
                    if seen[key] != e["episode_id"]:
                        raise ValueError("conflicting_duplicate_signal/"+slug)
                    diagnostics["duplicates_excluded"] += 1
                    continue
                seen[key] = e["episode_id"]
                signal = e["signal"]
                if signal.get("model_status")=="unavailable":
                    diagnostics["model_unavailable/"+(signal.get("model_unavailable_reason") or "unknown")] += 1
                    continue
                label = cache.get(slug, {})
                if label.get("confirmation") != "closed_terminal_prices" or not isinstance(label.get("winner_up"), bool):
                    diagnostics["missing_verified_label"] += 1
                    continue
                if e["window_end_ms"]-execution.timestamp(slug)*1000 != 300000 or "-updown-5m-" not in slug:
                    diagnostics["market_duration_not_5m"] += 1
                    continue
                if signal.get("twap_anchors", {}).get("topic") != "crypto_prices_twap_sixty":
                    diagnostics["incompatible_reference_topic"] += 1
                    continue
                try:
                    f = signal_features(signal, e["window_end_ms"])
                except ValueError as error:
                    diagnostics[str(error)] += 1
                    continue
                if f["horizon_secs"] < checkpoint*POLICY["min_horizon_checkpoint_ratio"]:
                    diagnostics["capture_late_for_checkpoint"] += 1
                    continue
                target = log_bps(label.get("final_price"), signal.get("current_twap"))
                opening_error = log_bps(signal.get("opening_twap"), label.get("price_to_beat"))
                gaussian = execution.probability(signal.get("probability_up"))
                if target is None:
                    diagnostics["missing_or_invalid_official_final_price"] += 1
                    continue
                if opening_error is None:
                    diagnostics["missing_or_invalid_official_opening_label"] += 1
                    continue
                if abs(opening_error)>POLICY["max_opening_error_bps"]:
                    diagnostics["official_opening_label_mismatch"] += 1
                    continue
                if gaussian is None:
                    diagnostics["missing_or_invalid_legacy_probability"] += 1
                    continue
                base = signal.get("baseline", {})
                market = execution.probability(base.get("probability_up")) if base.get("source") in ("up_midpoint", "down_midpoint") else None
                diagnostics["conditional_ready" if f["vector"] is not None else f["conditional_unavailable"]] += 1
                rows.append(dict(slug=slug, asset=e["asset"].lower(), checkpoint=checkpoint,
                                 time=execution.timestamp(slug), features=f, target_bps=target,
                                 gaussian=gaussian, market=market, won=int(label["winner_up"]), episode=e,
                                 opening_error_bps=opening_error))
        inputs.append(dict(path=str(path), sha256=digest.hexdigest(), labels_sha256=hashlib.sha256(cache_path.read_bytes()).hexdigest()))
    return sorted(rows, key=lambda r: (r["time"], r["asset"], r["checkpoint"])), dict(diagnostics), inputs, sorted(run_ids)


def score(rows, predictions):
    pairs = [(p, r["won"]) for r, p in zip(rows, predictions) if p is not None]
    if not pairs:
        return dict(samples=0, brier=None, log_loss=None, bins={})
    bins = collections.defaultdict(list)
    for p, y in pairs:
        for side, q, outcome in (("up", p, y), ("down", 1-p, 1-y)):
            bins[f"{side}/{min(9,int(q*10))}"].append((q, outcome))
    return dict(samples=len(pairs), brier=sum((p-y)**2 for p, y in pairs)/len(pairs),
                log_loss=-sum(math.log(max(1e-6, p if y else 1-p)) for p, y in pairs)/len(pairs),
                bins={key:dict(samples=len(v), mean_probability=sum(p for p,y in v)/len(v), win_rate=sum(y for p,y in v)/len(v)) for key,v in sorted(bins.items())})


def evaluate(rows, model, policy, name, stress=True):
    probabilities = [predict(r, model) if model else r["gaussian"] for r in rows]
    usable = [(r,p) for r,p in zip(rows, probabilities) if p is not None]
    paired = [(r,p) for r,p in usable if r["market"] is not None]
    reasons, trades = collections.Counter(), []
    for r, p in zip(rows, probabilities):
        reason = ("missing_market_midpoint" if model and model["kind"]=="market_offset_logistic" and r["market"] is None
                  else "missing_conditional_features" if model and model["kind"]!="asset_gaussian" and r["features"]["vector"] is None
                  else "terminal_features_or_horizon_unsupported")
        result = replay_with_fee(r,p,policy) if p is not None else dict(status="excluded", reason=reason)
        if result["status"] == "filled":
            ep=r["episode"]
            trades.append(dict(slug=r["slug"], asset=r["asset"], model=name,
                               entry_ms=ep["delayed"]["sampled_at_ms"],window_end_ms=ep["window_end_ms"],
                               episode_id=ep["episode_id"],**result))
        else:
            reasons[result["status"]+"/"+result["reason"]] += 1
    cost = sum(t["cost"]+t["fees"] for t in trades)
    pnl = sum(t["settlement_pnl"] for t in trades)
    daily = collections.defaultdict(float)
    for t in trades:
        day = dt.datetime.fromtimestamp(execution.timestamp(t["slug"]), dt.timezone.utc).date().isoformat()
        daily[day] += t["settlement_pnl"]
    delta = collections.defaultdict(list)
    for r, p in paired:
        day = dt.datetime.fromtimestamp(r["time"], dt.timezone.utc).date().isoformat()
        delta[day].append((p-r["won"])**2-(r["market"]-r["won"])**2)
    funded = None
    if "initial_balance" in policy:
        funded,trades,_ = risk.summarize(trades,rows,policy)
        cost,pnl = funded["cost"],funded["pnl"]
        daily = funded["daily_pnl"]
    trade_rows = []
    trade_probabilities = []
    by_slug = {r["slug"]:r for r in rows}
    for trade in trades:
        r = by_slug[trade["slug"]]
        trade_rows.append(r)
        q = trade["predicted_probability"]
        trade_probabilities.append(q if trade["side"]=="up" else 1-q)
    stress_value=None
    if stress and "initial_balance" in policy:
        stressed=dict(policy,slippage_bps=policy["stress_slippage_bps"],haircut=policy["stress_haircut"],
                      failure_rate=policy["stress_failure_rate"])
        stress_value,_=evaluate(rows,model,stressed,name,False)
        stress_value=dict(policy={k:stressed[k] for k in ("slippage_bps","haircut","failure_rate")},
                          execution=stress_value["execution"],funded_portfolio=stress_value["funded_portfolio"],
                          note="Replays the same linked >=1s books; cannot simulate an unobserved 2s book.")
    return dict(all=score(rows, probabilities), paired_model=score([r for r,p in paired], [p for r,p in paired]),
                paired_market=score([r for r,p in paired], [r["market"] for r,p in paired]),
                paired_legacy=score([r for r,p in paired], [r["gaussian"] for r,p in paired]),
                paired_daily_brier_delta={d:dict(samples=len(v), mean_delta=sum(v)/len(v)) for d,v in sorted(delta.items())},
                funded_portfolio=funded, trade_subset_model=score(trade_rows,trade_probabilities),
                cost_stress=stress_value,
                trade_subset_market=score([r for r in trade_rows if r["market"] is not None],
                                         [r["market"] for r in trade_rows if r["market"] is not None]),
                execution=dict(fills=len(trades), cost=cost, fees=sum(t["fees"] for t in trades), pnl=pnl,
                               expected_pnl=sum(t["expected_pnl"] for t in trades), roi=pnl/cost if cost else None,
                               wins=sum(t["settlement_pnl"]>0 for t in trades), daily_pnl=dict(sorted(daily.items())), reasons=dict(reasons))), trades


def study(rows, inputs, diagnostics, run_ids, frozen=None, next_round=False, portfolio_policy=None):
    if not rows:
        if (frozen and frozen.get("schema_version") in (1,2)
                and frozen.get("model_type") in ("sddfv_terminal_residual_v1","sddfv_terminal_residual_v2")
                and run_ids and any(key.startswith("model_unavailable/") for key in diagnostics)):
            report=dict(schema_version=1,generated_at=dt.datetime.now(dt.timezone.utc).isoformat(),
                purpose="capture diagnostics only; no usable model episodes",
                source_rows=0,diagnostics=diagnostics,inputs=inputs,groups={},
                split_boundaries=frozen["split_boundaries"],policy=frozen["policy"],
                fixed_model_replay=True,review_scope="capture_only_no_model_evidence",
                local_validation_passed=False,forward_validation_passed=False,
                online_trading_enabled=False,portfolio_scenarios={},
                limitations=["No usable labeled model episodes: PnL is unavailable, not zero-profit evidence."])
            return frozen,report,[]
        raise ValueError("no_valid_labeled_terminal_episodes")
    policy = dict(POLICY if frozen is None else frozen["policy"])
    next_round = next_round or (frozen is not None and frozen.get("schema_version")==2)
    if next_round and ({r["checkpoint"] for r in rows}!={30} or not {r["asset"] for r in rows}<={"btc","eth"}):
        raise ValueError("next_round_requires_btc_eth_5m_30s_only")
    if next_round and not frozen:
        policy.update(risk.DEFAULTS, fresh_min_fills=100, fresh_fee_source="observed_required",
                      fresh_requires_opening=True,stress_slippage_bps=20.0,stress_haircut=.50,stress_failure_rate=.04)
        policy.update(portfolio_policy or {})
        risk.validate(policy)
    elif portfolio_policy:
        raise ValueError("portfolio_settings_require_new_next_round_artifact")
    if frozen:
        if frozen.get("schema_version") not in (1,2) or frozen.get("model_type") not in ("sddfv_terminal_residual_v1","sddfv_terminal_residual_v2"):
            raise ValueError("incompatible_frozen_model")
        if not run_ids or any(r["time"] <= frozen["split_boundaries"]["data_end"] for r in rows) or set(run_ids)&set(frozen["run_ids"]):
            raise ValueError("frozen_validation_requires_new_later_batch")
        mapping = {r["time"]:"fresh_validation" for r in rows}
        bounds = frozen["split_boundaries"]
    else:
        mapping, bounds = execution.partition(rows)
    models, groups, trades = {}, {}, []
    for key in sorted({f"{r['asset']}/{r['checkpoint']}s" for r in rows}):
        subset = [r for r in rows if f"{r['asset']}/{r['checkpoint']}s" == key]
        splits = {name:[r for r in subset if mapping[r["time"]] == name] for name in sorted(set(mapping.values()))}
        candidates = {}
        train = splits.get("train", [])
        if frozen:
            if key not in frozen["models"]:
                groups[key] = dict(promotion_allowed=False, reasons=["group_not_in_frozen_model"])
                continue
            candidates = frozen["models"][key]["candidates"]
        elif next_round:
            base = fit(train,"conditional_empirical",10.0)
            if base:
                candidates["terminal_empirical_baseline"] = base
                for ridge in (10.0,100.0):
                    candidate = fit_offset(train,base,ridge)
                    if candidate:
                        candidates[f"market_offset_ridge_{ridge:g}"] = candidate
        else:
            base = fit(train, "asset_gaussian")
            if base:
                candidates["asset_gaussian"] = base
            for ridge in (10.0, 100.0):
                for kind in ("conditional_gaussian", "conditional_empirical"):
                    candidate = fit(train, kind, ridge)
                    if candidate:
                        candidates[f"{kind}_ridge_{ridge:g}"] = candidate
        details = {}
        for split, group in splits.items():
            details[split] = {}
            for name, model in dict(legacy_gaussian=None, **candidates).items():
                eval_policy = dict(policy)
                if frozen and next_round:
                    eval_policy["fee_source"] = policy["fresh_fee_source"]
                    eval_policy["require_opening_audit"] = policy["fresh_requires_opening"]
                value, records = evaluate(group, model, eval_policy, name)
                details[split][name] = value
                trades.extend(dict(record, split=split) for record in records)
        selected = frozen["models"][key]["selected"] if frozen else None
        if not frozen:
            acceptable = []
            for name in candidates:
                v = details.get("validation", {}).get(name, {})
                if (v and execution.improves(v["paired_model"], v["paired_market"])
                        and execution.improves(v["paired_model"], v["paired_legacy"])
                        and (not next_round or v["execution"]["pnl"]>0)):
                    acceptable.append(name)
            if acceptable:
                selected = min(acceptable, key=lambda name:details["validation"][name]["paired_model"]["log_loss"])
        models[key] = dict(selected=selected, candidates=candidates)
        reasons = []
        if selected is None:
            reasons.append("no_frozen_selected_candidate" if frozen else "no_validation_advantage_over_market_and_legacy")
        checks = {}
        for split in (("fresh_validation",) if frozen else ("validation", "holdout")):
            # Candidate metrics are evaluated even when selected=null. Never
            # describe a missing choice as zero actual observations.
            checks[split] = {name:candidate_gate(v,policy,split,next_round) for name,v in details.get(split,{}).items()}
            if selected is None:
                continue
            v = details.get(split, {}).get(selected, {})
            n = v.get("paired_model", {}).get("samples", 0)
            if n < (policy["min_validation"] if split == "validation" else policy["min_holdout"]):
                reasons.append(split+"_insufficient_paired_samples")
            if not v or not execution.improves(v["paired_model"], v["paired_market"]):
                reasons.append(split+"_not_better_than_market")
            x = v.get("execution", {})
            if x.get("fills", 0) < policy["min_execution_fills"]:
                reasons.append(split+"_insufficient_delayed_fills")
            if x.get("pnl", 0) <= 0:
                reasons.append(split+"_nonpositive_execution_pnl")
            daily = x.get("daily_pnl", {})
            if len(daily) < policy["min_span_days"] or sum(p>0 for p in daily.values()) <= len(daily)/2:
                reasons.append(split+"_insufficient_positive_daily_blocks")
            if next_round:
                reasons.extend(split+"_"+r for r in checks[split].get(selected,["selected_metrics_missing"]))
        local_passed = selected is not None and not reasons
        if not frozen:
            reasons.append("requires_new_post_freeze_batch")
        groups[key] = dict(selected=selected, promotion_allowed=not reasons,
                           local_validation_passed=local_passed,candidate_checks=checks,
                           reasons=sorted(set(reasons)), splits=details,
                           training_scales={name:{field:model[field] for field in (
                               "training_samples", "pooled_sigma_bps", "sigma_floor_bps",
                               "horizon_min_secs", "horizon_max_secs")} for name,model in candidates.items()},
                           review_scope="historical_data_already_inspected" if not frozen else "new_post_freeze_batch")
    artifact = frozen or dict(schema_version=2 if next_round else 1,
                              model_type="sddfv_terminal_residual_v2" if next_round else "sddfv_terminal_residual_v1", market_duration_secs=300, feature_names=FEATURES,
                              online_trading_enabled=False, run_ids=run_ids, inputs=inputs,
                              policy=policy, split_boundaries=bounds, models=models)
    report = dict(schema_version=1, generated_at=dt.datetime.now(dt.timezone.utc).isoformat(),
                  purpose="conditional terminal-return calibration; simulated execution only",
                  source_rows=len(rows), diagnostics=diagnostics, inputs=inputs, groups=groups,
                  split_boundaries=bounds, online_trading_enabled=False,
                  policy=policy,fixed_model_replay=bool(frozen),review_scope="new_post_freeze_batch" if frozen else "historical_development_data_already_reviewed",
                  limitations=["Existing historical data cannot establish a new prospective advantage.",
                               "Different candidate models are alternative scenarios, never additive profits.",
                               "One-sided markets are excluded from paired market comparisons, not imputed.",
                               "Static fees, 1s linked books and modeled failures are not actual execution.",
                               "Terminal scale is fitted at each checkpoint, with no sqrt-time extrapolation."])
    if next_round:
        portfolio_scenarios={}
        for split in sorted({t["split"] for t in trades}):
            portfolio_scenarios[split]={}
            for name in sorted({t["model"] for t in trades}):
                selected_trades=[t for t in trades if t["split"]==split and t["model"]==name]
                subset=[r for r in rows if mapping[r["time"]]==split]
                value,_,_=risk.summarize(selected_trades,subset,policy)
                portfolio_scenarios[split][name]=value
        report["portfolio_scenarios"]=portfolio_scenarios
        report["local_validation_passed"]=bool(groups) and all(g.get("local_validation_passed",False) for g in groups.values())
        report["forward_validation_passed"]=bool(frozen) and bool(groups) and all(g.get("promotion_allowed",False) for g in groups.values())
        report["limitations"] += ["Finite capital uses modelled end+120s cash release, not observed on-chain redemption.",
                                  "Full-day bootstrap intervals are exploratory and cannot cure post-hoc selection.",
                                  "TWAP weighting is not verified; captured spot buckets are audit-only."]
    return artifact, report, trades


def candidate_gate(value,policy,split,next_round=False):
    reasons=[]
    if value["paired_model"]["samples"] < policy["min_validation"]:
        reasons.append("insufficient_actual_paired_samples")
    if not execution.improves(value["paired_model"],value["paired_market"]):
        reasons.append("not_better_than_same_sample_market")
    x=value["execution"]
    minimum=policy.get("fresh_min_fills",policy["min_execution_fills"]) if split=="fresh_validation" else policy["min_execution_fills"]
    if x["fills"] < minimum:
        reasons.append("insufficient_actual_delayed_fills")
    if x["pnl"]<=0:
        reasons.append("nonpositive_net_pnl")
    if next_round:
        funded=value["funded_portfolio"]
        if len(funded["full_covered_days"]) < policy["min_span_days"]:
            reasons.append("insufficient_complete_daily_blocks")
        interval=funded["full_day_pnl_interval"]
        if not interval["available"] or interval["lower"]<=0:
            reasons.append("daily_pnl_interval_not_positive")
        if value["cost_stress"]["execution"]["pnl"]<=0:
            reasons.append("nonpositive_cost_stress_pnl")
    return reasons


def check_online_predictions(rows, model, sha):
    count, error = 0, 0.0
    for row in rows:
        audit = row["episode"]["signal"].get("terminal_model") or {}
        if audit.get("sha256") != sha or audit.get("decision_role") != "research_only":
            raise ValueError("fresh_batch_terminal_model_hash_or_role_mismatch")
        group = model["models"].get(f"{row['asset']}/{row['checkpoint']}s")
        if group is None:
            continue
        for name, candidate in group["candidates"].items():
            expected = predict(row, candidate)
            actual = number(audit.get("candidates", {}).get(name, {}).get("probability_up"))
            if expected is None and actual is None:
                continue
            if expected is None or actual is None or abs(expected-actual)>1e-6:
                raise ValueError("fresh_batch_online_offline_probability_mismatch/"+name)
            error = max(error, abs(expected-actual))
            count += 1
    if count == 0:
        raise ValueError("fresh_batch_has_no_matching_online_forecasts")
    return dict(forecasts=count, max_probability_error=error, passed=True, model_sha256=sha)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", nargs="+", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--model", help="replay a fixed artifact without refitting; model presence alone does not prove freeze/start")
    parser.add_argument("--next-round",action="store_true",help="BTC/ETH finite-capital market-offset development, schema v2")
    parser.add_argument("--symbols",default=None,help="comma-separated assets, fixed before evaluation")
    parser.add_argument("--initial-balance",type=float,help="virtual budget; new --next-round study only")
    parser.add_argument("--max-exposure",type=float,help="total locked-cost limit; new artifact only")
    parser.add_argument("--max-asset-exposure",type=float,help="per-asset locked-cost limit; new artifact only")
    args = parser.parse_args()
    output = Path(args.output)
    if (output/"model.json").exists():
        parser.error("output contains a frozen model; choose a new output directory")
    rows, diagnostics, inputs, run_ids = load(args.input)
    symbols = set((args.symbols or ("btc,eth" if args.next_round else "btc,eth,sol")).split(","))
    if not symbols or not symbols <= {"btc","eth","sol"}:
        parser.error("symbols must be btc,eth,sol")
    rows = [r for r in rows if r["asset"] in symbols]
    frozen = json.loads(Path(args.model).read_text()) if args.model else None
    portfolio={key:getattr(args,key) for key in ("initial_balance","max_exposure","max_asset_exposure") if getattr(args,key) is not None}
    model, report, trades = study(rows, inputs, diagnostics, run_ids, frozen,args.next_round,portfolio)
    report["capture_audit"]=capture_audit.summarize(args.input)
    if args.model:
        capture_audit.apply_context(report,capture_audit.review_context(args.input,args.model))
    encoded = json.dumps(model, sort_keys=True, indent=2, allow_nan=False)+"\n"
    report["model_sha256"] = hashlib.sha256(encoded.encode()).hexdigest()
    if args.model:
        frozen_sha = hashlib.sha256(Path(args.model).read_bytes()).hexdigest()
        report["online_inference_audit"] = (check_online_predictions(rows, model, frozen_sha) if rows else
            dict(forecasts=0,passed=False,status="unavailable_no_usable_model_episodes",model_sha256=frozen_sha))
    output.mkdir(parents=True, exist_ok=True)
    (output/"model.json").write_text(encoded)
    (output/"report.json").write_text(json.dumps(report, sort_keys=True, indent=2, allow_nan=False)+"\n")
    (output/"trades.ndjson").write_text("".join(json.dumps(t, sort_keys=True, allow_nan=False)+"\n" for t in trades))
    # Input-only inference cases permit full Rust/Python parity checks without labels.
    fixture_keys = ("sampled_at_ms", "seconds_remaining", "opening_twap", "current_twap",
                    "model_audit", "reference_timing", "twap_anchors", "spot_asof","baseline")
    with (output/"inference-fixture.ndjson").open("w") as stream:
        for row in rows:
            group = model["models"].get(f"{row['asset']}/{row['checkpoint']}s")
            if group is None:
                continue
            sig = row["episode"]["signal"]
            case = dict(asset=row["asset"], checkpoint=row["checkpoint"], window_end_ms=row["episode"]["window_end_ms"],
                        signal={key:sig.get(key) for key in fixture_keys}, model_sha256=report["model_sha256"],
                        expected={key:predict(row,candidate) for key,candidate in group["candidates"].items()})
            stream.write(json.dumps(case, sort_keys=True, allow_nan=False)+"\n")
    lines = ["# SDDFV terminal-distribution study", "", f"Rows: {len(rows)}. All fills/PnL are simulations.",
             f"Model SHA256: {report['model_sha256']}", "",
             "| Group | Selected | Promotion |", "|---|---|---|"]
    for key, value in report["groups"].items():
        lines.append(f"| {key} | {value.get('selected') or 'none'} | {value['promotion_allowed']} |")
        print(f"{key}: selected={value.get('selected')} promotion={value['promotion_allowed']}")
    lines += ["", f"Review scope: {report['review_scope']}. Model presence does NOT prove a profitable frozen validation.", "",
              "| Group | Split | Model | Paired N | Model Brier | Market Brier | Fills | Net simulated PnL |",
              "|---|---|---|---:|---:|---:|---:|---:|"]
    for key, group in report["groups"].items():
        for split, values in group.get("splits", {}).items():
            if split == "train":
                continue
            for name, v in values.items():
                brier, market = v["paired_model"]["brier"], v["paired_market"]["brier"]
                lines.append("| {} | {} | {} | {} | {} | {} | {} | {:.2f} |".format(
                    key, split, name, v["paired_model"]["samples"],
                    f"{brier:.5f}" if brier is not None else "NA",
                    f"{market:.5f}" if market is not None else "NA",
                    v["execution"]["fills"], v["execution"]["pnl"]))
    lines += ["", "See report.json for calibration bins, daily blocks and exclusions. "
              "Candidate scenarios must never be added together."]
    (output/"report.md").write_text("\n".join(lines)+"\n")
    print(f"Terminal report: {output/'report.json'} | rows={len(rows)} | sha256={report['model_sha256']}")


if __name__ == "__main__":
    main()
