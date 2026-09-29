#!/usr/bin/env python3
"""Offline, settlement-linked market residual study; never submits orders.

Train only on chronological train rows. Select on validation, never holdout.
Execution uses frozen signal probabilities and the linked >=1s delayed asks.
Old journals without linked depth cannot supply PnL or pass the promotion gate.
"""
import argparse
import collections
import datetime as dt
import hashlib
import json
import math
from pathlib import Path

FEATURES = ["intercept", "gaussian_minus_market", "directional_margin_bps"]
POLICY = dict(budget=5.0, fee_rate=0.07, fee_exponent=1, slippage_bps=10.0,
              failure_rate=0.02, haircut=0.75, min_fill_ratio=0.25,
              min_edge=0.03, min_price=0.05, max_price=0.95,
              min_delay_ms=1000, max_delay_ms=2000, min_train=75,
              min_validation=25, min_holdout=50, min_execution_fills=50,
              min_span_days=5, max_correction=0.10, max_rest_latency_ms=2000,
              max_book_age_ms=1000, max_reference_age_ms=5000,
              max_local_age_ms=2000, max_reference_transport_ms=2000)


def finite(x):
    try:
        v = float(x)
        return v if math.isfinite(v) else None
    except (ValueError, TypeError):
        return None


def probability(x):
    v = finite(x)
    return v if v is not None and 0 <= v <= 1 else None


def timestamp(slug):
    try:
        return int(slug.rsplit("-", 1)[1])
    except (IndexError, ValueError):
        raise ValueError(f"market start timestamp missing: {slug}") from None


def features(row):
    # Fixed feature definition, clipped before fitting. No settlement features.
    return [1.0, row["gaussian"] - row["market"],
            max(-100.0, min(100.0, row["margin"])) / 10.0]


def logit(p):
    p = max(1e-6, min(1 - 1e-6, p))
    return math.log(p / (1 - p))


def sigmoid(x):
    return 1 / (1 + math.exp(-max(-35.0, min(35.0, x))))


def predict(row, model):
    if not any(model["beta"]):
        return row["market"]
    z = [(x - mean) / scale for x, mean, scale in
         zip(features(row), model["means"], model["scales"])]
    p = sigmoid(logit(row["market"]) + sum(a * b for a, b in zip(z, model["beta"])))
    cap = model["max_correction"]
    return max(0.0, min(1.0, max(row["market"] - cap, min(row["market"] + cap, p))))


def fit(rows, ridge, cap=0.10):
    xs = [features(r) for r in rows]
    means = [0.0] + [sum(x[j] for x in xs) / len(xs) for j in (1, 2)]
    scales = [1.0] + [max(0.01, math.sqrt(sum((x[j] - means[j]) ** 2 for x in xs) / len(xs)))
                      for j in (1, 2)]
    zs = [[(x - m) / s for x, m, s in zip(row, means, scales)] for row in xs]
    beta = [0.0] * len(FEATURES)
    # Convex offset-logistic ridge fit. Fixed iterations/learning rate, train only.
    for _ in range(600):
        grad = [ridge * b / len(rows) for b in beta]
        for r, z in zip(rows, zs):
            error = sigmoid(logit(r["market"]) + sum(a * b for a, b in zip(beta, z))) - r["won"]
            for j in range(len(beta)):
                grad[j] += error * z[j] / len(rows)
        beta = [b - 0.1 * g for b, g in zip(beta, grad)]
    return dict(beta=beta, means=means, scales=scales, ridge=ridge, max_correction=cap)


def scores(rows, forecast):
    if not rows:
        return dict(samples=0, brier=None, log_loss=None, high_probability={})
    pairs = [(forecast(r), r["won"]) for r in rows]
    bins = {}
    for side in ("up", "down"):
        group = [(p if side == "up" else 1-p, y if side == "up" else 1-y)
                 for p, y in pairs if (p if side == "up" else 1-p) >= 0.9]
        bins[side] = dict(samples=len(group), mean_probability=sum(p for p, _ in group)/len(group) if group else None,
                          win_rate=sum(y for _, y in group)/len(group) if group else None)
    return dict(samples=len(rows), brier=sum((p-y)**2 for p, y in pairs)/len(rows),
                log_loss=-sum(y*math.log(max(p, 1e-12))+(1-y)*math.log(max(1-p, 1e-12))
                              for p, y in pairs)/len(rows), high_probability=bins)


def improves(candidate, baseline):
    return (candidate["samples"] > 0 and candidate["samples"] == baseline["samples"]
            and candidate["brier"] < baseline["brier"]
            and candidate["log_loss"] < baseline["log_loss"])


def partition(rows):
    times = sorted({r["time"] for r in rows})
    a, b = len(times)*60//100, len(times)*80//100
    mapping = {t: "train" if i < a else "validation" if i < b else "holdout"
               for i, t in enumerate(times)}
    return mapping, dict(train_end=times[a-1] if a else None,
                         validation_end=times[b-1] if b else None,
                         data_end=times[-1] if times else None)


def load(paths):
    rows, episodes, diagnostics, seen = {}, {}, collections.Counter(), []
    for path in paths:
        path = Path(path)
        labels_path = path.parent / "reference-audit.json"
        labels = json.loads(labels_path.read_text()) if labels_path.exists() else {}
        digest = hashlib.sha256()
        local_observations, local_episodes = {}, {}
        with path.open("rb") as handle:
            for line_no, line in enumerate(handle, 1):
                digest.update(line)
                if not line.strip():
                    continue
                try:
                    e = json.loads(line)
                except json.JSONDecodeError as error:
                    raise ValueError(f"{path}:{line_no}: invalid journal: {error}") from None
                kind = e.get("event")
                if kind not in ("model_observation", "residual_execution_observation", "residual_capture_rejected"):
                    continue
                diagnostics[kind] += 1
                if kind == "residual_capture_rejected":
                    continue
                slug, checkpoint = e["slug"], e["checkpoint_secs"]
                key = (slug, checkpoint)
                if kind == "residual_execution_observation":
                    local_episodes.setdefault(key, e)
                else:
                    old = local_observations.get(key)
                    if old is None or (probability(old.get("market_probability_up")) is None
                                       and probability(e.get("market_probability_up")) is not None):
                        local_observations[key] = e
        for key in local_observations.keys() | local_episodes.keys():
            slug, checkpoint = key
            ep = local_episodes.get(key)
            e = ep["signal"] if ep else local_observations[key]
            # Episodes freeze the actual signal. Never substitute a later snapshot.
            base = e.get("baseline") or e.get("market_baseline") or {}
            m = probability(base.get("probability_up", e.get("market_probability_up")))
            g = probability(e.get("probability_up"))
            source = base.get("source", "up_midpoint")
            diagnostics[f"baseline/{source}"] += 1
            if m is None or g is None:
                diagnostics["missing_signal_probability"] += 1
                continue
            label = labels.get(slug, {})
            if not isinstance(label.get("winner_up"), bool) or label.get("confirmation") != "closed_terminal_prices":
                diagnostics["missing_verified_label"] += 1
                continue
            record = ep or local_observations[key]
            margin = finite(e.get("model_audit", {}).get("directional_margin_bps"))
            if margin is None:
                diagnostics["missing_signal_features"] += 1
                continue
            if key in rows:
                diagnostics["duplicate_markets_ignored"] += 1
                continue
            rows[key] = dict(slug=slug, asset=record["asset"].lower(), checkpoint=checkpoint,
                             time=timestamp(slug), market=m, gaussian=g, margin=margin,
                             won=int(label["winner_up"]), source=source, episode=ep)
            if ep:
                episodes[key] = ep
        seen.append(dict(path=str(path), sha256=digest.hexdigest(), labels=str(labels_path),
                         labels_sha256=hashlib.sha256(labels_path.read_bytes()).hexdigest() if labels_path.exists() else None))
    return sorted(rows.values(), key=lambda r: (r["time"], r["asset"])), diagnostics, seen


def quality(ep, policy):
    if ep is None:
        return "missing_linked_delayed_book"
    signal, delayed = ep["signal"], ep["delayed"]
    delta = finite(delayed.get("actual_delay_ms"))
    monotonic = finite(delayed.get("monotonic_delay_ms"))
    start, end = finite(signal.get("sampled_at_ms")), finite(delayed.get("sampled_at_ms"))
    if (delta is None or monotonic is None or start is None or end is None
            or abs(end - start - delta) > 2 or abs(monotonic - delta) > 50
            or not policy["min_delay_ms"] <= delta <= policy["max_delay_ms"]):
        return "delay_outside_1_2s_or_clock_jump"
    if not delayed.get("before_expiry") or end >= ep["window_end_ms"]:
        return "delayed_after_expiry"
    a = signal.get("model_audit", {})
    for key, cap in [("observation_age_ms", policy["max_reference_age_ms"]),
                     ("local_receive_age_ms", policy["max_local_age_ms"]),
                     ("network_receive_latency_ms", policy["max_reference_transport_ms"])]:
        value = finite(a.get(key))
        if value is None or not 0 <= value <= cap:
            return "reference_stale_or_missing/" + key
    return None


def book_valid(book, now, policy):
    http = book.get("http", {})
    stamp, latency = finite(http.get("exchange_timestamp")), finite(http.get("rest_latency_ms"))
    return (book.get("valid") is True and stamp is not None and latency is not None
            and 0 <= now - stamp <= policy["max_book_age_ms"]
            and 0 <= latency <= policy["max_rest_latency_ms"])


def fee(price, p):
    return p["fee_rate"] * (price * (1-price)) ** p["fee_exponent"]


def asks(book):
    result = []
    for level in book.get("asks", []):
        price, size = finite(level.get("price")), finite(level.get("size"))
        if price is not None and size is not None and 0 < price <= 1 and size > 0:
            result.append((price, size))
    return sorted(result)


def replay(row, predicted, p):
    ep = row.get("episode")
    error = quality(ep, p)
    if error:
        return dict(status="excluded", reason=error)
    signal, delayed = ep["signal"], ep["delayed"]
    candidates = []
    for side, prob in [("up", predicted), ("down", 1-predicted)]:
        book = signal["books"][side]
        levels = asks(book)
        if not book_valid(book, signal["sampled_at_ms"], p) or not levels:
            continue
        price = levels[0][0] * (1+p["slippage_bps"]/10000)
        if not p["min_price"] <= price <= p["max_price"]:
            continue
        edge = prob - price - fee(price, p)
        if edge >= p["min_edge"]:
            candidates.append((edge, side, prob))
    if not candidates:
        return dict(status="rejected", reason="no_signal_edge")
    _, side, prob = max(candidates)
    book = delayed["books"][side]
    if not book_valid(book, delayed["sampled_at_ms"], p):
        return dict(status="excluded", reason="delayed_selected_book_invalid")
    levels = asks(book)
    if not levels:
        return dict(status="rejected", reason="delayed_no_asks")
    # Stable common failure draw for baseline/model, independent of labels.
    draw = int(hashlib.sha256((ep["episode_id"]+"/order-failure").encode()).hexdigest()[:16], 16)/2**64
    if draw < p["failure_rate"]:
        return dict(status="rejected", reason="simulated_order_failure")
    shares, spend, fees, limit_price = 0.0, 0.0, 0.0, None
    for raw, size in levels:
        price = raw * (1+p["slippage_bps"]/10000)
        if not p["min_price"] <= price <= p["max_price"]:
            break
        f = fee(price, p)
        if prob - price - f < p["min_edge"]:
            break  # delayed deterioration, never reforecast on later reference
        quantity = min(size*p["haircut"], (p["budget"]-spend-fees)/(price+f))
        if quantity <= 0:
            break
        shares += quantity
        spend += quantity*price
        fees += quantity*f
        limit_price = price
    if shares == 0 or (spend+fees)/p["budget"] + 1e-10 < p["min_fill_ratio"]:
        return dict(status="rejected", reason="insufficient_delayed_edge_or_partial_depth")
    payout = shares * (row["won"] if side == "up" else 1-row["won"])
    return dict(status="filled", side=side, shares=shares, cost=spend, fees=fees,
                average_price=spend/shares, limit_price=limit_price,
                partial_fill_ratio=(spend+fees)/p["budget"], predicted_probability=prob,
                expected_pnl=shares*prob-spend-fees, settlement_pnl=payout-spend-fees,
                delay_ms=delayed["actual_delay_ms"], simulated_only=True)


def executions(rows, model, p):
    records, reasons = [], collections.Counter()
    valid_observations = 0
    for row in rows:
        result = replay(row, predict(row, model), p)
        valid_observations += result["status"] != "excluded"
        if result["status"] == "filled":
            records.append(dict(slug=row["slug"], asset=row["asset"], **result))
        else:
            reasons[result["status"]+"/"+result["reason"]] += 1
    cost = sum(r["cost"]+r["fees"] for r in records)
    pnl = sum(r["settlement_pnl"] for r in records)
    # Contiguous UTC-date groups, not falsely independent repeated snapshots.
    daily = collections.defaultdict(float)
    for r in records:
        date = dt.datetime.fromtimestamp(timestamp(r["slug"]), dt.timezone.utc).date().isoformat()
        daily[date] += r["settlement_pnl"]
    return dict(samples=len(rows), independent_fills=len(records), cost=cost,
                quality_valid_observations=valid_observations,
                settlement_pnl=pnl if valid_observations else None,
                roi=pnl/cost if cost else None,
                reasons=dict(reasons), daily_pnl=dict(daily)), records


def study(rows, inputs, diagnostics, frozen=None, policy=None):
    p = dict(POLICY if policy is None else policy)
    if frozen:
        if {r["checkpoint"] for r in rows} != {frozen["checkpoint_secs"]}:
            raise ValueError("frozen model checkpoint differs from new batch")
        p = frozen["policy"]
        mapping = {r["time"]: "fresh_validation" for r in rows}
        bounds = frozen["split_boundaries"]
        if any(r["time"] <= bounds["data_end"] for r in rows):
            raise ValueError("frozen validation overlaps fitting data; use a NEW later batch")
        if any(i["sha256"] in {j["sha256"] for j in frozen["inputs"]} for i in inputs):
            raise ValueError("frozen validation reuses fitting journal")
    else:
        mapping, bounds = partition(rows)
    report, models, trades = {}, {}, []
    for asset in sorted({r["asset"] for r in rows}):
        subset = [r for r in rows if r["asset"] == asset and r["source"] in ("up_midpoint", "down_midpoint")]
        partitions = {s: [r for r in subset if mapping[r["time"]] == s]
                      for s in sorted(set(mapping.values()))}
        train, val = partitions.get("train", []), partitions.get("validation", [])
        # No absolute-price prediction; zero coefficients exactly recover market.
        baseline_model = dict(beta=[0.0]*len(FEATURES), means=[0.0]*len(FEATURES),
                              scales=[1.0]*len(FEATURES), ridge=None, max_correction=p["max_correction"])
        selected, choice = baseline_model, "market_baseline_no_validated_residual"
        selection = []
        if frozen:
            if asset not in frozen["assets"]:
                report[asset] = dict(promotion_allowed=False, reasons=["asset_not_in_frozen_model"])
                continue
            selected = frozen["assets"][asset]
            choice = selected["selection"]
        elif len(train) >= p["min_train"] and len(val) >= p["min_validation"]:
            base = scores(val, lambda r: r["market"])
            best = base
            for ridge in (1.0, 10.0, 100.0):
                model = fit(train, ridge, p["max_correction"])
                score = scores(val, lambda r: predict(r, model))
                selection.append(dict(ridge=ridge, validation=score))
                if improves(score, base) and score["brier"] < best["brier"]:
                    selected, best, choice = model, score, f"offset_logistic_ridge_{ridge}"
        models[asset] = dict(selected, selection=choice)
        detail, execution = {}, {}
        for split, group in partitions.items():
            detail[split] = dict(market=scores(group, lambda r:r["market"]),
                                 residual=scores(group, lambda r:predict(r, selected)),
                                 gaussian=scores(group, lambda r:r["gaussian"]))
            run, records = executions(group, selected, p)
            base_run, _ = executions(group, baseline_model, p)
            execution[split] = dict(residual=run, market=base_run)
            trades.extend(dict(r, split=split) for r in records)
        reasons = []
        if choice == "market_baseline_no_validated_residual":
            reasons.append("no_validation_prediction_improvement")
        checks = ("fresh_validation",) if frozen else ("validation", "holdout")
        for split in checks:
            score = detail.get(split, {})
            if not score or not improves(score["residual"], score["market"]):
                reasons.append(split+"_not_better_than_same_sample_market")
            minimum = p["min_validation"] if split == "validation" else p["min_holdout"]
            if len(partitions.get(split, [])) < minimum:
                reasons.append(split+"_insufficient_prediction_samples")
            x = execution.get(split, {}).get("residual", {})
            baseline_x = execution.get(split, {}).get("market", {})
            if x.get("independent_fills", 0) < p["min_execution_fills"]:
                reasons.append(split+"_insufficient_delayed_fills")
            if (x.get("settlement_pnl") or 0) <= 0 or (x.get("settlement_pnl") or 0) <= (baseline_x.get("settlement_pnl") or 0):
                reasons.append(split+"_nonpositive_or_no_incremental_net_pnl")
            days = len(x.get("daily_pnl", {}))
            if days < p["min_span_days"] or sum(v > 0 for v in x.get("daily_pnl", {}).values()) < math.ceil(days*0.6):
                reasons.append(split+"_insufficient_positive_daily_blocks")
        if not frozen:
            reasons.append("requires_new_post_freeze_batch")
        report[asset] = dict(selection=choice, selection_candidates=selection,
                             paired_predictions=detail, executions=execution,
                             interval_baseline_samples_excluded=sum(r["asset"] == asset and r["source"] == "complementary_ask_interval" for r in rows),
                             promotion_allowed=not reasons, reasons=reasons)
    artifact = dict(schema_version=1, model_kind="market_offset_logistic_residual",
                    feature_names=FEATURES, checkpoint_secs=rows[0]["checkpoint"],
                    policy=p, split_boundaries=bounds, inputs=inputs,
                    assets=models, training_method="train_fit_validation_select_holdout_once_no_refit",
                    live_deployment_supported=False)
    summary = dict(schema_version=1, generated_at=dt.datetime.now(dt.timezone.utc).isoformat(),
                   purpose="fresh_frozen_validation" if frozen else "chronological_research_fit",
                   unique_labeled_rows=len(rows), diagnostics=dict(diagnostics), inputs=inputs,
                   split_boundaries=bounds, policy=p, assets=report,
                   online_trading_enabled=False,
                   limitations=["simulated execution, not actual fills", "static fee assumptions; confirm per-market fee before deployment",
                                "ask intervals excluded from primary midpoint comparisons", "linked missing data never imputed as profitable trades",
                                "daily blocks are diagnostics, not a statistically certified profit guarantee"])
    return artifact, summary, trades


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", action="append", required=True, help="events.ndjson (repeat for disjoint batches)")
    parser.add_argument("--output", required=True)
    parser.add_argument("--model", help="frozen model.json: evaluate ONLY new later data, never refit")
    args = parser.parse_args()
    rows, diagnostics, inputs = load(args.input)
    if not rows:
        raise SystemExit("No labeled observations: run calibrate --refresh first; verify signal features/baselines")
    if len({r["checkpoint"] for r in rows}) != 1:
        raise SystemExit("Use one checkpoint per study; 10s/30s must not be mixed")
    frozen = json.loads(Path(args.model).read_text()) if args.model else None
    if frozen and (frozen["schema_version"] != 1 or frozen["feature_names"] != FEATURES):
        raise SystemExit("Incompatible frozen residual model")
    artifact, report, trades = study(rows, inputs, diagnostics, frozen)
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    report["frozen_model_sha256"] = hashlib.sha256(Path(args.model).read_bytes()).hexdigest() if args.model else None
    # Existing freeze artifacts must not be silently replaced by later refits.
    model_path = output / "model.json"
    if model_path.exists() and json.loads(model_path.read_text()) != artifact:
        raise SystemExit("output contains a different frozen model; choose a NEW output directory")
    model_path.write_text(json.dumps(artifact, indent=2, allow_nan=False)+"\n")
    report["model_sha256"] = hashlib.sha256(model_path.read_bytes()).hexdigest()
    (output / "report.json").write_text(json.dumps(report, indent=2, allow_nan=False)+"\n")
    with (output / "trades.ndjson").open("w") as handle:
        for trade in trades:
            handle.write(json.dumps(trade, allow_nan=False)+"\n")
    md = ["# Market residual study", "", "Research-only. No live order promotion.", "",
          "| Asset | Selected | Holdout n | Residual Brier | Market Brier | Gate |",
          "| --- | --- | ---: | ---: | ---: | --- |"]
    for asset, result in report["assets"].items():
        score = result.get("paired_predictions", {}).get("holdout", result.get("paired_predictions", {}).get("fresh_validation", {}))
        m, r = score.get("market", {}), score.get("residual", {})
        md.append(f"| {asset} | {result.get('selection')} | {r.get('samples')} | {r.get('brier')} | {m.get('brier')} | {'pass' if result['promotion_allowed'] else 'closed'} |")
    md.extend(["", "Simulated execution (not actual fills):", "",
               "| Asset | Split | Valid fills | Cost + fees | Settlement PnL |",
               "| --- | --- | ---: | ---: | ---: |"])
    for asset, result in report["assets"].items():
        for split, execution in result.get("executions", {}).items():
            run = execution["residual"]
            net = f"{run['settlement_pnl']:.4f}" if run['settlement_pnl'] is not None else "N/A (missing evidence)"
            md.append(f"| {asset} | {split} | {run['independent_fills']} | {run['cost']:.4f} | {net} |")
    md.append("")
    for asset, result in report["assets"].items():
        md.extend([f"{asset}: "+", ".join(result["reasons"]), ""])
    (output / "report.md").write_text("\n".join(md)+"\n")
    print(f"Residual report: {output/'report.json'} | unique labeled rows={len(rows)} | online trading=false")
    for asset, result in report["assets"].items():
        print(f"{asset}: {result.get('selection')} | gate={'pass' if result['promotion_allowed'] else 'closed'}")


if __name__ == "__main__":
    main()
