#!/usr/bin/env python3
"""Read-only ETH probability calibration development. No network or live orders.

All reviewed run-011 data are DEVELOPMENT data, including walk-forward scores.
Training uses signal-eligible midpoint windows, including no-edge/unfilled ones.
Only raw UP probability (and market UP for shrinkage) enters either calibrator.
"""
import argparse
import collections
import datetime as dt
import hashlib
import json
import math
import random
import statistics
from pathlib import Path

import digital_fair_value_terminal_study as terminal

ROOT = Path(__file__).resolve().parents[1]
BASE_SHA = "09460671790e82e45a8e3512b4f6817d56bfd0376ce18bacabc77f6aac486af6"
BASE_CANDIDATE = "terminal_empirical_baseline"
METHODS = ("identity", "logistic", "market_shrink")
SETTINGS = dict(min_train=100, min_class=10, min_training_days=3,
                label_embargo_ms=300000, logistic_ridge=1.0,
                slope_bounds=[0.05, 5.0], intercept_bounds=[-5.0, 5.0],
                bootstrap_draws=4000, bootstrap_seed=20261019,
                minimum_review_fills=50, minimum_complete_days=5)


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def stamp(value):
    return int(dt.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000)


def utc_day(seconds):
    return dt.datetime.fromtimestamp(seconds, dt.timezone.utc).date().isoformat()


def day_start(day):
    return stamp(day + "T00:00:00+00:00")


def sigmoid(z):
    return terminal.execution.sigmoid(z)


def logit(p):
    return terminal.execution.logit(p)


def fit_logistic(samples, ridge=1.0):
    """Convex penalized log loss, shrink (slope,intercept) towards identity.

    No profit, chosen side, delayed quote, activity or future features are read.
    Bounds and penalty are fixed a priori; no hyperparameter search.
    """
    if not samples:
        raise ValueError("empty_training_samples")
    xy = [(logit(r["p"]), r["y"]) for r in samples]

    def objective(a, b):
        return sum(max(a*x+b, 0) - y*(a*x+b) + math.log1p(math.exp(-abs(a*x+b)))
                   for x, y in xy) + 0.5*ridge*((a-1)**2+b*b)

    a, b = 1.0, 0.0
    for _ in range(100):
        ps = [sigmoid(a*x+b) for x, _ in xy]
        ga = sum((p-y)*x for (x, y), p in zip(xy, ps)) + ridge*(a-1)
        gb = sum(p-y for (_, y), p in zip(xy, ps)) + ridge*b
        weights = [max(1e-12, p*(1-p)) for p in ps]
        aa = sum(w*x*x for (x, _), w in zip(xy, weights)) + ridge
        ab = sum(w*x for (x, _), w in zip(xy, weights))
        bb = sum(weights) + ridge
        det = aa*bb-ab*ab
        if det <= 0:
            raise ValueError("singular_logistic_hessian")
        da, db = (bb*ga-ab*gb)/det, (aa*gb-ab*ga)/det
        before = objective(a, b)
        for exponent in range(30):
            scale = 0.5**exponent
            na = max(0.05, min(5.0, a-scale*da))
            nb = max(-5.0, min(5.0, b-scale*db))
            if objective(na, nb) <= before + 1e-12:
                break
        else:
            break
        change = max(abs(na-a), abs(nb-b))
        a, b = na, nb
        if change < 1e-10:
            break
    return dict(method="logistic", slope=a, intercept=b, ridge=ridge,
                training_samples=len(samples), penalized_log_loss=objective(a, b))


def fit_shrink(samples):
    """One-parameter Brier optimum: m + alpha*(p-m), alpha in [0,1]."""
    if not samples or any(r["market"] is None for r in samples):
        raise ValueError("shrink_requires_nonempty_market_paired_training")
    numerator = sum((r["p"]-r["market"])*(r["y"]-r["market"]) for r in samples)
    denominator = sum((r["p"]-r["market"])**2 for r in samples)
    # With no identifiable model-market difference, retain identity.
    alpha = max(0.0, min(1.0, numerator/denominator)) if denominator > 1e-12 else 1.0
    return dict(method="market_shrink", alpha=alpha, training_samples=len(samples),
                objective="Brier", identifiable=denominator > 1e-12)


def forecast(sample, model):
    if model["method"] == "identity":
        return sample["p"]
    if model["method"] == "logistic":
        return sigmoid(model["slope"]*logit(sample["p"])+model["intercept"])
    if model["method"] == "market_shrink":
        if sample["market"] is None:
            return None
        return sample["market"] + model["alpha"]*(sample["p"]-sample["market"])
    raise ValueError("unknown_calibrator")


def signal_eligibility(row, policy):
    """Pre-edge, signal-only gate; never inspects delayed snapshot or outcome."""
    p, reason = terminal.causal_signal_policy(row, policy)
    if p is None:
        return reason
    signal = row["episode"]["signal"]
    audit = signal.get("model_audit", {})
    for key, limit in [("observation_age_ms", p["max_reference_age_ms"]),
                       ("local_receive_age_ms", p["max_local_age_ms"]),
                       ("network_receive_latency_ms", p["max_reference_transport_ms"])]:
        value = terminal.number(audit.get(key))
        if value is None or not 0 <= value <= limit:
            return "reference_stale_or_missing/" + key
    for side in ("up", "down"):
        book = signal["books"][side]
        asks = terminal.execution.asks(book)
        if terminal.execution.book_valid(book, signal["sampled_at_ms"], p) and asks:
            price = asks[0][0]*(1+p["slippage_bps"]/10000)
            if p["min_price"] <= price <= p["max_price"]:
                return None
    return "no_signal_side_in_price_range"


def market_eligibility(row, policy):
    """Do not fit shrinkage on a stale or inconsistent logged midpoint."""
    if row["market"] is None:
        return "missing_market_midpoint"
    signal = row["episode"]["signal"]
    source = signal.get("baseline", {}).get("source")
    side = {"up_midpoint": "up", "down_midpoint": "down"}.get(source)
    if side is None:
        return "unsupported_midpoint_source"
    book = signal["books"][side]
    if not terminal.execution.book_valid(book, signal["sampled_at_ms"], policy):
        return "stale_signal_midpoint"
    bid, ask = terminal.number(book.get("best_bid")), terminal.number(book.get("best_ask"))
    if bid is None or ask is None or not 0 < bid <= ask < 1:
        return "invalid_signal_midpoint_quotes"
    midpoint = (bid+ask)/2 if side == "up" else 1-(bid+ask)/2
    return None if abs(midpoint-row["market"]) <= 1e-9 else "midpoint_quote_mismatch"


def signal_choice(row, probability, policy):
    p, _ = terminal.causal_signal_policy(row, policy)
    if p is None:
        return None
    signal = row["episode"]["signal"]
    choices = []
    for side, q in (("up", probability), ("down", 1-probability)):
        book = signal["books"][side]
        asks = terminal.execution.asks(book)
        if not terminal.execution.book_valid(book, signal["sampled_at_ms"], p) or not asks:
            continue
        price = asks[0][0]*(1+p["slippage_bps"]/10000)
        edge = q-price-terminal.execution.fee(price, p)
        if p["min_price"] <= price <= p["max_price"] and edge >= p["min_edge"]:
            choices.append((edge, side, q))
    if not choices:
        return None
    edge, side, q = max(choices)
    return dict(side=side, q=q, edge=edge)


def scoring(pairs):
    """One observation per market; selected orientation supplied by caller."""
    if not pairs:
        return dict(n=0, brier=None, log_loss=None, calibration_gap=None, bins={})
    bins = collections.defaultdict(list)
    for p, y in pairs:
        bins[min(4, int(p*5))].append((p, y))
    n = len(pairs)
    return dict(n=n, mean_probability=sum(p for p, _ in pairs)/n,
                observed_frequency=sum(y for _, y in pairs)/n,
                calibration_gap=sum(p-y for p, y in pairs)/n,
                brier=sum((p-y)**2 for p, y in pairs)/n,
                log_loss=-sum(math.log(max(1e-12, p if y else 1-p)) for p, y in pairs)/n,
                bins={str(k): dict(n=len(v), mean_probability=sum(p for p, _ in v)/len(v),
                                   observed_frequency=sum(y for _, y in v)/len(v))
                      for k, v in sorted(bins.items())})


def training_for_day(records, day):
    cutoff = day_start(day)
    return [r for r in records if r["shared"] and r["signal_ms"] < cutoff
            and r["known_at_ms"] is not None and r["known_at_ms"] <= cutoff
            and r["end_ms"]+SETTINGS["label_embargo_ms"] <= cutoff]


def sufficient_training(samples):
    counts = collections.Counter(r["y"] for r in samples)
    return (len(samples) >= SETTINGS["min_train"]
            and min(counts.get(0, 0), counts.get(1, 0)) >= SETTINGS["min_class"]
            and len({r["day"] for r in samples}) >= SETTINGS["min_training_days"])


def rolling_forecasts(records):
    folds, predictions = [], {}
    for day in sorted({r["day"] for r in records}):
        train = training_for_day(records, day)
        status = "evaluated" if sufficient_training(train) else "insufficient_past_training"
        fold = dict(day=day, fit_at_ms=day_start(day), status=status, training_samples=len(train),
                    maximum_training_known_at_ms=max((r["known_at_ms"] for r in train), default=None),
                    maximum_training_window_end_ms=max((r["end_ms"] for r in train), default=None),
                    training_days=sorted({r["day"] for r in train}))
        folds.append(fold)
        if status != "evaluated":
            continue
        models = dict(identity=dict(method="identity"),
                      logistic=fit_logistic(train, SETTINGS["logistic_ridge"]),
                      market_shrink=fit_shrink(train))
        fold["calibrators"] = models
        for r in records:
            if r["day"] == day:
                predictions[r["slug"]] = {k: forecast(r, model) if r["shared"] else None
                                           for k, model in models.items()}
    return folds, predictions


def complete_days(records):
    coverage = collections.defaultdict(set)
    for r in records:
        coverage[r["day"]].add(r["time"] % 86400)
    return sorted(day for day, slots in coverage.items()
                  if 0 in slots and 86100 in slots and len(slots) >= 274)


def execute(records, probabilities, policy):
    trades, reasons, choices = [], collections.Counter(), {}
    for r in records:
        p = probabilities.get(r["slug"])
        if p is None:
            reasons["unsupported_common_cohort"] += 1
            continue
        choice = signal_choice(r["row"], p, policy)
        if choice and signal_eligibility(r["row"], policy) is None:
            choices[r["slug"]] = choice
        result = terminal.replay_with_fee(r["row"], p, policy)
        if result["status"] != "filled":
            reasons[result.get("reason", result["status"])] += 1
            continue
        ep = r["row"]["episode"]
        trades.append(dict(result, slug=r["slug"], asset="eth", entry_ms=ep["delayed"]["sampled_at_ms"],
                           window_end_ms=r["end_ms"], day=r["day"], y_up=r["y"]))
    account = terminal.risk.replay(trades, policy)
    funded = account.pop("trades")
    account.pop("events")
    winners = sorted((t["settlement_pnl"] for t in funded if t["settlement_pnl"] > 0), reverse=True)
    total = sum(t["settlement_pnl"] for t in funded)
    days = sorted({r["day"] for r in records})
    daily = {day: sum(t["settlement_pnl"] for t in funded if t["day"] == day) for day in days}
    full = [day for day in complete_days(records) if day in days]
    score = scoring([(probabilities[t["slug"]] if t["side"] == "up" else 1-probabilities[t["slug"]],
                      t["y_up"] if t["side"] == "up" else 1-t["y_up"]) for t in funded])
    by_slug = {r["slug"]: r for r in records}
    signal_score = scoring([(choice["q"], by_slug[slug]["y"] if choice["side"] == "up"
                             else 1-by_slug[slug]["y"]) for slug, choice in choices.items()])
    summary = dict(fills=len(funded), pnl=total, wins=sum(t["settlement_pnl"] > 0 for t in funded),
                   fees=sum(t["fees"] for t in funded), selected_probability_score=score,
                   signal_edge_count=len(choices), signal_edge_probability_score=signal_score,
                   daily_pnl=daily, full_covered_days=full,
                   full_day_pnl_interval=terminal.risk.interval_values(
                       [daily[day] for day in full], SETTINGS["bootstrap_draws"]),
                   all_observed_day_pnl_interval=terminal.risk.interval_values(
                       [daily[day] for day in days], SETTINGS["bootstrap_draws"]),
                   largest_win=winners[0] if winners else 0,
                   pnl_without_largest_win_sensitivity=total-(winners[0] if winners else 0),
                   account=account, rejections=dict(reasons))
    return summary, funded, choices


def paired_review(records, predictions, reference_sides, method, full_days):
    pairs = []
    for r in records:
        side = reference_sides.get(r["slug"])
        p = predictions.get(r["slug"], {}).get(method)
        if side is None or p is None:
            continue
        raw, market, y = r["p"], r["market"], r["y"]
        if side == "down":
            p, raw, market, y = 1-p, 1-raw, 1-market, 1-y
        pairs.append(dict(day=r["day"], p=p, raw=raw, market=market, y=y))
    def endpoint(rs):
        if not rs:
            return None
        return dict(brier_minus_raw=sum((r["p"]-r["y"])**2-(r["raw"]-r["y"])**2 for r in rs)/len(rs),
                    brier_minus_market=sum((r["p"]-r["y"])**2-(r["market"]-r["y"])**2 for r in rs)/len(rs),
                    calibration_gap=sum(r["p"]-r["y"] for r in rs)/len(rs))
    result = dict(n=len(pairs), reference="fixed_raw_selected_contract_not_candidate_selected",
                  calibrated=scoring([(r["p"], r["y"]) for r in pairs]),
                  raw=scoring([(r["raw"], r["y"]) for r in pairs]),
                  market=scoring([(r["market"], r["y"]) for r in pairs]),
                  all_observed=endpoint(pairs),
                  complete_days=endpoint([r for r in pairs if r["day"] in full_days]),
                  uncertainty=dict(available=False, reason="fewer_than_five_complete_evaluation_days"))
    by_day = {day: [r for r in pairs if r["day"] == day] for day in full_days}
    if len(full_days) >= 5 and any(by_day.values()):
        rng = random.Random(SETTINGS["bootstrap_seed"])
        draws = []
        for _ in range(SETTINGS["bootstrap_draws"]):
            value = endpoint([r for day in rng.choices(full_days, k=len(full_days)) for r in by_day[day]])
            if value is not None:
                draws.append(value)
        ci = dict(available=True, blocks=len(full_days), complete_days=full_days,
                  method="exploratory paired UTC-day bootstrap; not independent validation")
        for key in ("brier_minus_raw", "brier_minus_market", "calibration_gap"):
            values = sorted(d[key] for d in draws)
            ci[key] = dict(lower=values[int(.025*len(values))],
                           upper=values[min(len(values)-1, int(.975*len(values)))])
        result["uncertainty"] = ci
    return result, pairs


def size_estimate(pairs, full_days, target=0.02):
    """Planning approximation; few historical clusters cannot guarantee power."""
    means = []
    counts = []
    for day in full_days:
        rs = [r for r in pairs if r["day"] == day]
        if rs:
            means.append(statistics.mean((r["p"]-r["y"])**2-(r["raw"]-r["y"])**2 for r in rs))
            counts.append(len(rs))
    if len(means) < 5:
        return dict(available=False, reason="too_few_nonempty_day_clusters", not_a_power_guarantee=True)
    sd = statistics.stdev(means)
    days = max(10, math.ceil(((1.96+0.84)*sd/target)**2))
    return dict(available=True, method="approximate day-mean normal planning, 80% power, two-sided 5%",
                target_brier_improvement=target, historical_nonempty_days=len(means), daily_difference_sd=sd,
                estimated_complete_days=days, paired_fill_floor=max(200, math.ceil(days*statistics.median(counts))),
                proposed_fixed_duration_days=max(14, days+2),
                not_a_power_guarantee=True, requires_review_before_future_data=True)


def build_dataset(paths, model, model_sha, policy, fixed_cohort):
    plans = []
    for path in paths:
        plan_path = Path(path).parent/"run-plan.json"
        plan = json.loads(plan_path.read_text())
        if plan.get("mode") != "observe" or plan.get("model_sha256") != model_sha:
            raise ValueError("changed_source_mode_or_base_model")
        plans.append(dict(path=str(plan_path), sha256=sha(plan_path), plan=plan))
    loaded, diagnostics, inputs, runs = terminal.load(paths)
    if set(runs) & set(model["run_ids"]):
        raise ValueError("base_model_development_run_overlap")
    cutoff = model["split_boundaries"]["data_end"]*1000
    records, excluded = [], collections.Counter()
    labels = {}
    for path in paths:
        cache = json.loads((Path(path).parent/"reference-audit.json").read_text())
        for slug, value in cache.items():
            if slug in labels and labels[slug] != value:
                raise ValueError("conflicting_label_caches/"+slug)
            labels[slug] = value
    for row in loaded:
        if row["asset"] != "eth" or row["checkpoint"] != 30:
            continue
        if row["time"]*1000 <= cutoff:
            raise ValueError("base_model_development_time_overlap")
        base = model["models"]["eth/30s"]["candidates"][BASE_CANDIDATE]
        p = terminal.predict(row, base)
        if p is None:
            excluded["unsupported_base_forecast"] += 1
            continue
        signal = row["episode"]["signal"]
        logged = signal.get("terminal_model") or {}
        value = (logged.get("candidates") or {}).get(BASE_CANDIDATE, {}).get("probability_up")
        if logged.get("sha256") != model_sha or value is None or abs(value-p) > 1e-12:
            raise ValueError("frozen_online_forecast_mismatch/"+row["slug"])
        when = labels[row["slug"]].get("verified_at")
        known = stamp(when) if when else None
        if known is not None and known < row["episode"]["window_end_ms"]:
            raise ValueError("label_verified_before_terminal_window/"+row["slug"])
        signal_reason = signal_eligibility(row, policy)
        market_reason = market_eligibility(row, policy)
        reason = signal_reason or market_reason
        if reason:
            excluded[reason] += 1
        records.append(dict(slug=row["slug"], time=row["time"], day=utc_day(row["time"]),
                            signal_ms=signal["sampled_at_ms"], end_ms=row["episode"]["window_end_ms"],
                            p=p, market=row["market"], y=row["won"], known_at_ms=known,
                            shared=reason is None, exclusion=reason, row=row))
    records.sort(key=lambda r: (r["time"], r["slug"]))
    cohort = {}
    lookup = {r["slug"]: r for r in records}
    if fixed_cohort:
        for line in Path(fixed_cohort).open():
            e = json.loads(line)
            if e["result"]["status"] != "filled":
                continue
            row = lookup.get(e["slug"])
            if row is None or abs(row["p"]-e["p_up"]) > 1e-12 or row["y"] != e["y_up"]:
                raise ValueError("fixed_diagnostic_cohort_not_same_input/"+e["slug"])
            if e["slug"] in cohort:
                raise ValueError("duplicate_fixed_diagnostic_cohort")
            cohort[e["slug"]] = e["choice"]["side"]
    if not records:
        raise ValueError("no_supported_eth_records")
    return records, dict(load=diagnostics, exclusions=dict(excluded), source_inputs=inputs,
                         source_run_plans=plans,
                         base_development_end_ms=cutoff, base_run_ids_disjoint=True,
                         fixed_diagnostic_cohort_size=len(cohort)), cohort


def run_study(records, policy, fixed_cohort=None):
    folds, forecasts = rolling_forecasts(records)
    evaluated = [r for r in records if r["slug"] in forecasts]
    full = complete_days(evaluated)
    strategies, trades, reference, stress_trades = {}, [], {}, []
    stress = dict(policy, failure_rate=policy["stress_failure_rate"], haircut=policy["stress_haircut"],
                  slippage_bps=policy["stress_slippage_bps"])
    # A full-support raw control makes abstention/market coverage changes visible.
    for method in ("identity_full_support",) + METHODS:
        ps = {r["slug"]: r["p"] if method == "identity_full_support" else forecasts[r["slug"]][method]
              for r in evaluated}
        summary, ts, choices = execute(evaluated, ps, policy)
        stressed, sts, _ = execute(evaluated, ps, stress)
        summary["stress"] = stressed
        summary["up_probability_score"] = scoring([(ps[r["slug"]], r["y"]) for r in evaluated
                                                     if ps[r["slug"]] is not None])
        strategies[method] = summary
        trades.extend(dict(t, method=method, scenario="base") for t in ts)
        stress_trades.extend(dict(t, method=method, scenario="stress") for t in sts)
        if method == "identity":
            reference = {t["slug"]: t["side"] for t in ts}
    comparisons, selected_pairs = {}, {}
    for method in METHODS:
        review, pairs = paired_review(evaluated, forecasts, reference, method, full)
        fixed, _ = paired_review(evaluated, forecasts, fixed_cohort or {}, method, full)
        fixed["original_cohort_size"] = len(fixed_cohort or {})
        fixed["not_evaluated_or_outside_common_support"] = len(fixed_cohort or {})-fixed["n"]
        comparisons[method] = dict(raw_funded_reference=review, original_diagnostic_cohort=fixed)
        selected_pairs[method] = pairs
    candidates = []
    reasons = {}
    for method in METHODS[1:]:
        review = comparisons[method]["raw_funded_reference"]
        score = strategies[method]["up_probability_score"]
        rawscore = strategies["identity"]["up_probability_score"]
        ci = review["uncertainty"]
        failures = []
        if review["n"] < SETTINGS["minimum_review_fills"]:
            failures.append("fewer_than_50_reference_fills")
        if len(full) < SETTINGS["minimum_complete_days"]:
            failures.append("fewer_than_5_complete_evaluation_days")
        if not ci["available"] or ci["brier_minus_raw"]["upper"] >= 0:
            failures.append("paired_reference_brier_improvement_interval_not_negative")
        if score["n"] == 0 or score["brier"] >= rawscore["brier"]:
            failures.append("common_universe_brier_not_improved")
        if score["n"] == 0 or score["log_loss"] > rawscore["log_loss"]:
            failures.append("common_universe_log_loss_worse")
        reasons[method] = failures
        if not failures:
            candidates.append(method)
    # Fixed ranking rule: primary endpoint, with method name only as a tie-break.
    selected = min(candidates, key=lambda k: (comparisons[k]["raw_funded_reference"]
                                             ["all_observed"]["brier_minus_raw"], k)) if candidates else None
    report = dict(scope="reviewed_historical_development_only_not_independent_validation",
                  settings=SETTINGS, folds=folds, complete_evaluation_days=full,
                  supported_windows=len(records), common_support_windows=sum(r["shared"] for r in records),
                  evaluated_windows=len(evaluated), evaluated_common_windows=sum(r["shared"] for r in evaluated),
                  strategies=strategies, comparisons=comparisons,
                  selection=dict(method=selected, reasons=reasons, role="probability_research_only",
                                 trading_accepted=False, online_promotion=False),
                  label_availability="Only verified_at <= fold midnight, plus terminal+5min embargo. "
                                     "Cache verification may be later than true first availability; conservative development replay.",
                  notes=["No live model or policy changes; no network or orders.",
                         "Shared training pool includes no-edge and unfilled signal-eligible midpoint windows.",
                         "Both methods use identical training rows; missing midpoint means common-cohort abstention.",
                         "Brier measures resolution as well as calibration; inspect reliability and log loss.",
                         "Changed-selection PnL is separately recomputed with finite-capital base and stress ledgers.",
                         "Historical rolling results and candidate selection are development, never a new holdout."])
    report["future_sample_size_estimate"] = size_estimate(selected_pairs[selected], full) if selected else dict(
        available=False, reason="no_development_candidate_passed")
    return report, trades+stress_trades, forecasts


def future_protocol(report, fitted, artifact_sha):
    method = report["selection"]["method"]
    estimate = report["future_sample_size_estimate"]
    blocking = []
    if method is None:
        blocking.append("no_probability_candidate_passed_development_screen")
    elif report["strategies"][method]["fills"] < SETTINGS["minimum_review_fills"]:
        blocking.append("insufficient_reselected_strategy_fills")
    if method == "market_shrink" and fitted.get(method, {}).get("alpha") == 0:
        blocking.append("frozen_calibrator_is_market_probability_not_model_edge")
    blocking.extend(report["selection"].get("strategy_development_blockers", []))
    return dict(schema_version=1, status="draft_not_started", purpose="probability_validation_only",
                candidate=method, calibrators_sha256=artifact_sha, base_model_sha256=BASE_SHA,
                candidate_available=method in fitted if method else False,
                launch_ready=False, blocking_reasons=sorted(set(blocking)),
                start_utc=None, end_utc=None,
                required_before_start=["Freeze one calibrator and input hashes before viewing new labels.",
                                       "Finalize UTC start/end, sample counts and practical improvement target.",
                                       "Collect under observe with the unchanged causal data and cost policy."],
                primary="same_raw_funded_selected_contracts_calibrated_brier_minus_raw_brier",
                primary_direction="negative; prespecified complete-UTC-day paired interval upper < 0",
                supporting=["selected reliability gap and bins", "log loss", "same-sample market Brier",
                            "new-selection coverage, finite-capital net/stress PnL, drawdown, concentration"],
                sample_size_estimate=estimate, fixed_end_required=True, no_extension_to_pass=True,
                reviewed_data_may_not_be_future_validation=True, trading_acceptance=False,
                original_100_fills_5_days_diagnostic_gate_not_reused=True)


def markdown(report):
    lines = ["# ETH 概率校准离线开发评估", "",
             "已查看历史数据的滚动开发结果；不是独立验证，不改变运行模型。", "",
             f"支持窗口 {report['supported_windows']}，共同支持窗口 {report['common_support_windows']}；"
             f"滚动评估窗口 {report['evaluated_windows']}，共同支持 {report['evaluated_common_windows']}。", "",
             "## 重新选单与有限资金回放", "",
             "各方法是替代情景，不能相加。identity_full_support 用于识别报价覆盖导致的变化。", "",
             "| 方法 | 成交 | 净 PnL | 压力 PnL | UP Brier |", "|---|---:|---:|---:|---:|"]
    for name, value in report["strategies"].items():
        brier = value["up_probability_score"]["brier"]
        lines.append(f"| {name} | {value['fills']} | {value['pnl']:+.2f} | "
                     f"{value['stress']['pnl']:+.2f} | {brier if brier is not None else '不足'} |")
    lines += ["", "## 同一原始成交子集上的概率比较", "",
              "固定原始方向与标签，不使用各候选自行选出的有利子集。负的 Brier 差表示改善。", "",
              "| 方法 | 成对 N | Brier 差（校准−原始） | 完整日探索性区间 |",
              "|---|---:|---:|---|"]
    for name, value in report["comparisons"].items():
        q = value["raw_funded_reference"]
        delta = q["all_observed"]["brier_minus_raw"] if q["all_observed"] else None
        lines.append(f"| {name} | {q['n']} | {delta} | {q['uncertainty'].get('brier_minus_raw', '不足')} |")
    lines += ["", "## 训练、边界与下一阶段", "",
              "只用当时已验证的过去标签，在 UTC 日初拟合；市场窗口终值另加 5 分钟隔离。"
              "标签审计时间可能晚于首次可用时间，因此此处按已知审计时间做保守限制。", "",
              "逻辑校准与市场收缩使用相同的信号时合格训练窗口；包含未成交、无 edge 窗口。"
              "拟合不使用成交结果、未来报价、PnL 或辅助特征。", "",
              f"开发候选：{report['selection']['method']}。原因：{report['selection']['reasons']}。", "",
              "这是概率研究筛选结果，不是交易候选验收。市场收缩若退化为完全采用市场概率，"
              "且重新选单后零成交，只能作为基准诊断，不能称为可交易的校准成功。", "",
              "即使概率改善，也不意味着策略有正期望。不能通过不成交或改变样本集合掩盖失准。", "",
              "prospective-protocol.json 是未启动草案。所有历史数据保持开发属性，"
              "新一轮需先冻结一个方案及期限，再收集未查看的数据；不沿用旧诊断数量门槛作为交易验收。", "",
              "- [完整结果](report.json)", "- [滚动预测](forecasts.ndjson)",
              "- [全部基础/压力模拟成交](trades.ndjson)", "- [最终拟合参数](fitted-calibrators.json)",
              "- [下一轮草案](prospective-protocol.json)", "- [数据与代码哈希](manifest.json)", ""]
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", nargs="+", required=True)
    parser.add_argument("--model", default=str(ROOT/"results/short-duration-digital-fair-value/market-offset-009-development-v2/model.json"))
    parser.add_argument("--fixed-cohort", help="Prior prospective episodes.ndjson; used only for scoring")
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    output = Path(args.output)
    if output.exists():
        parser.error("choose a NEW output directory")
    model_path = Path(args.model)
    if sha(model_path) != BASE_SHA:
        parser.error("fixed base model hash mismatch")
    model = json.loads(model_path.read_text())
    policy = dict(model["policy"], fee_source="observed_required", require_opening_audit=True)
    records, audit, cohort = build_dataset(args.input, model, BASE_SHA, policy, args.fixed_cohort)
    report, trades, forecasts = run_study(records, policy, cohort)
    now = dt.datetime.now(dt.timezone.utc)
    train = [r for r in records if r["shared"] and r["known_at_ms"] is not None
             and r["known_at_ms"] <= now.timestamp()*1000
             and r["end_ms"]+SETTINGS["label_embargo_ms"] <= now.timestamp()*1000]
    fitted = dict(identity=dict(method="identity"))
    if sufficient_training(train):
        fitted.update(logistic=fit_logistic(train, SETTINGS["logistic_ridge"]), market_shrink=fit_shrink(train))
    artifact = dict(schema_version=1, base_model_sha256=BASE_SHA, base_candidate=BASE_CANDIDATE,
                    asset="eth", checkpoint_secs=30, fitted_at_utc=now.isoformat(),
                    calibrators=fitted, training_samples=len(train),
                    maximum_training_known_at_ms=max((r["known_at_ms"] for r in train), default=None),
                    training_sample_ids_sha256=hashlib.sha256(
                        "\n".join(sorted(r["slug"] for r in train)).encode()).hexdigest(),
                    development_inputs=audit["source_inputs"],
                    development_scope="reviewed_historical_data_only",
                    support="signal_eligible_fresh_midpoint_only", policy=policy, settings=SETTINGS,
                    online_trading_enabled=False, selected_for_probability_research=report["selection"]["method"])
    selected = report["selection"]["method"]
    collapsed = selected == "market_shrink" and fitted.get(selected, {}).get("alpha") == 0
    report["selection"]["frozen_is_market_reference"] = collapsed
    blockers = []
    if selected:
        strategy = report["strategies"][selected]
        report["selection"]["reselected_fills"] = strategy["fills"]
        report["selection"]["reselected_stress_pnl"] = strategy["stress"]["pnl"]
        if strategy["fills"] < SETTINGS["minimum_review_fills"]:
            blockers.append("insufficient_reselected_strategy_fills")
        if strategy["pnl"] <= 0:
            blockers.append("nonpositive_reselected_net_pnl")
        if strategy["stress"]["pnl"] <= 0:
            blockers.append("nonpositive_reselected_stress_pnl")
        interval = strategy["full_day_pnl_interval"]
        if not interval["available"] or interval["lower"] <= 0:
            blockers.append("reselected_daily_pnl_interval_not_positive")
    else:
        blockers.append("no_probability_candidate_passed_development_screen")
    if collapsed:
        blockers.append("frozen_calibrator_is_market_probability_not_model_edge")
    report["selection"]["strategy_development_blockers"] = blockers
    report["selection"]["executable_strategy_found"] = selected is not None and not blockers
    # Reconcile the unchanged raw replay against the independently archived
    # diagnostic ledger. Warmup days outside rolling evaluation are not scored.
    verification = dict(base_model_hash_verified=True, no_base_training_overlap=True,
                        causal_label_availability_checked=True, raw_replay_matches_archived_cohort=None)
    if args.fixed_cohort:
        archived = {e["slug"]: e for e in (json.loads(l) for l in Path(args.fixed_cohort).open())
                    if e["result"]["status"] == "filled"}
        raw = {t["slug"]: t for t in trades if t["method"] == "identity_full_support" and t["scenario"] == "base"}
        expected = set(archived) & set(forecasts)
        if expected - set(raw):
            raise ValueError("archived_raw_fills_missing_in_replay")
        for slug in expected:
            original, current = archived[slug]["result"], raw[slug]
            if original["side"] != current["side"] or any(
                    not math.isclose(original[k], current[k], abs_tol=1e-9)
                    for k in ("shares", "cost", "fees", "settlement_pnl", "predicted_probability")):
                raise ValueError("raw_replay_changed/"+slug)
        verification.update(raw_replay_matches_archived_cohort=True, reconciled_archived_fills=len(expected),
                            original_diagnostic_fills=len(archived),
                            warmup_or_unsupported_diagnostic_fills=len(archived)-len(expected),
                            archived_total_pnl=sum(e["result"]["settlement_pnl"] for e in archived.values()),
                            archived_rolling_overlap_pnl=sum(archived[s]["result"]["settlement_pnl"] for s in expected))
    for item in audit["source_inputs"]:
        if sha(item["path"]) != item["sha256"] or sha(Path(item["path"]).parent/"reference-audit.json") != item["labels_sha256"]:
            raise ValueError("input_changed_during_research")
    for item in audit["source_run_plans"]:
        if sha(item["path"]) != item["sha256"]:
            raise ValueError("source_run_plan_changed_during_research")
    if sha(model_path) != BASE_SHA:
        raise ValueError("base_model_changed_during_research")
    verification["source_and_label_hashes_unchanged"] = True
    report["data_audit"] = audit
    report["fixed_policy"] = policy
    report["base_model_sha256"] = BASE_SHA
    output.mkdir(parents=True)
    def write(name, value):
        (output/name).write_text(json.dumps(value, indent=2, sort_keys=True, allow_nan=False)+"\n")
    write("report.json", report)
    write("fitted-calibrators.json", artifact)
    protocol = future_protocol(report, fitted, sha(output/"fitted-calibrators.json"))
    if selected:
        candidate = {k: v for k, v in artifact.items() if k != "calibrators"}
        candidate.update(calibrator=fitted[selected], role="market_reference_only" if collapsed
                         else "probability_research_only", trading_accepted=False)
        write("candidate.json", candidate)
        protocol["candidate_sha256"] = sha(output/"candidate.json")
    write("verification.json", verification)
    write("prospective-protocol.json", protocol)
    compact = [{k: v for k, v in r.items() if k != "row"} for r in records]
    for name, values in [("dataset.ndjson", compact), ("trades.ndjson", trades),
                         ("forecasts.ndjson", [dict(slug=r["slug"], day=r["day"], y=r["y"],
                                                   raw_p_up=r["p"], market_p_up=r["market"],
                                                   probabilities=forecasts[r["slug"]])
                                                  for r in records if r["slug"] in forecasts])]:
        (output/name).write_text("".join(json.dumps(v, sort_keys=True, allow_nan=False)+"\n" for v in values))
    dependencies = [Path(__file__), Path(terminal.__file__), Path(terminal.execution.__file__),
                    Path(terminal.risk.__file__)]
    (output/"analysis.zh-CN.md").write_text(markdown(report))
    write("manifest.json", dict(created_at_utc=now.isoformat(), inputs=audit["source_inputs"],
                                 base_model=dict(path=str(model_path), sha256=BASE_SHA),
                                 fixed_cohort=None if not args.fixed_cohort else dict(
                                     path=args.fixed_cohort, sha256=sha(args.fixed_cohort)),
                                 code_sha256={str(p): sha(p) for p in dependencies},
                                 outputs_sha256={p.name: sha(p) for p in output.iterdir() if p.is_file()},
                                 research_only=True, no_online_promotion=True))
    print(json.dumps(dict(output=str(output), supported=len(records),
                          evaluated=report["evaluated_windows"], selection=report["selection"]), indent=2))


if __name__ == "__main__":
    main()
