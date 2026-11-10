#!/usr/bin/env python3
"""Frozen-plan ETH stage, incremental-information and cashflow diagnostics.

Consumes reviewed calibration development artifacts. Never fits a trade model,
fetches labels, sends orders or starts an observation run. A negative incremental
coefficient is a diagnostic result, not permission to reverse a trading signal.
"""
import argparse
import collections
import datetime as dt
import hashlib
import json
import math
import random
from pathlib import Path

import digital_fair_value_probability_calibration as c


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True, allow_nan=False)+"\n")


def interval(values):
    if not values:
        return dict(available=False, reason="no_nonempty_resamples")
    values = sorted(values)
    return dict(available=True, lower=values[int(.025*len(values))],
                upper=values[min(len(values)-1, int(.975*len(values)))])


def means(rows, keys):
    return {k: sum(r[k] for r in rows)/len(rows) if rows else None for k in keys}


def day_intervals(rows, days, keys, plan):
    """One common sampled UTC-day bag per draw; zero-row days are retained."""
    if len(days) < 5:
        return dict(available=False, reason="fewer_than_five_complete_days", complete_days=days)
    stats = {day: (sum(r["day"] == day for r in rows),
                   {k: sum(r[k] for r in rows if r["day"] == day) for k in keys}) for day in days}
    rng = random.Random(plan["bootstrap_seed"])
    draws = {k: [] for k in keys}
    for _ in range(plan["bootstrap_draws"]):
        selected = rng.choices(days, k=len(days))
        n = sum(stats[d][0] for d in selected)
        if n:
            for key in keys:
                draws[key].append(sum(stats[d][1][key] for d in selected)/n)
    return dict(available=bool(draws[keys[0]]), complete_days=days, blocks=len(days),
                metrics={k: interval(v) for k, v in draws.items()},
                method="exploratory paired UTC-day bootstrap on reviewed development data")


def stage_contrast(left, right, days, plan):
    def stat(rs, day):
        use = [r for r in rs if r["day"] == day]
        return len(use), sum(r["optimism"] for r in use)
    a, b = {d: stat(left, d) for d in days}, {d: stat(right, d) for d in days}
    rng = random.Random(plan["bootstrap_seed"])
    draws = []
    if len(days) >= 5:
        for _ in range(plan["bootstrap_draws"]):
            selected = rng.choices(days, k=len(days))
            na, nb = sum(a[d][0] for d in selected), sum(b[d][0] for d in selected)
            if na and nb:
                draws.append(sum(a[d][1] for d in selected)/na-sum(b[d][1] for d in selected)/nb)
    return dict(left_n=len(left), right_n=len(right),
                optimism_mean_difference=(means(left, ["optimism"])["optimism"]-
                                          means(right, ["optimism"])["optimism"]) if left and right else None,
                interval=interval(draws), interpretation="conditional association, not causal attribution")


def best_opportunity(row, probability, policy):
    """Pick one side BEFORE thresholding; same ranking as the original replay."""
    pol, evidence = c.terminal.causal_signal_policy(row, policy)
    if pol is None:
        raise ValueError("unsupported_causal_signal/"+evidence)
    signal = row["episode"]["signal"]
    opportunities = []
    for side, q in (("up", probability), ("down", 1-probability)):
        book = signal["books"][side]
        asks = c.terminal.execution.asks(book)
        if not c.terminal.execution.book_valid(book, signal["sampled_at_ms"], pol) or not asks:
            continue
        raw_ask = asks[0][0]
        price = raw_ask*(1+pol["slippage_bps"]/10000)
        fee = c.terminal.execution.fee(price, pol)
        if pol["min_price"] <= price <= pol["max_price"]:
            opportunities.append((q-price-fee, side, q, raw_ask, price, fee))
    if not opportunities:
        raise ValueError("shared_window_without_eligible_signal_side")
    edge, side, q, raw_ask, price, fee = max(opportunities)
    return dict(side=side, q=q, edge=edge, signal_raw_ask=raw_ask,
                signal_price_with_slippage=price, signal_fee_per_share=fee)


def disagreement_bin(value, edges):
    # Negative differences remain in the first group, never silently discarded.
    return "lt_010" if value < edges[0] else "010_025" if value < edges[1] else "ge_025"


def stage_summary(rows, days, plan):
    keys = ["q", "m", "y", "claimed_advantage", "realized_advantage", "optimism", "brier_minus_market"]
    return dict(n=len(rows), means=means(rows, keys),
                model_score=c.scoring([(r["q"], r["y"]) for r in rows]),
                market_score=c.scoring([(r["m"], r["y"]) for r in rows]),
                daily={d: dict(n=sum(r["day"] == d for r in rows),
                              means=means([r for r in rows if r["day"] == d], keys))
                       for d in sorted({r["day"] for r in rows})},
                complete_day_intervals=day_intervals(rows, days,
                    ["claimed_advantage", "realized_advantage", "optimism", "brier_minus_market"], plan))


def fit_market_offset(samples, with_increment, plan):
    """Fixed ridge log-loss diagnostic: logit(m)+b+beta*(p-m)/0.10.

    Only the single model discrepancy is added. The market-intercept control
    gets the identical intercept penalty. Both signs of beta are permitted.
    """
    dim = 2 if with_increment else 1
    scale, ridge = plan["disagreement_scale"], plan["ridge"]
    xy = [(c.logit(r["market"]), [1.0, (r["p"]-r["market"])/scale][:dim], r["y"])
          for r in samples]
    if not xy:
        raise ValueError("empty_increment_training")
    def objective(beta):
        total = .5*ridge*sum(b*b for b in beta)
        for offset, x, y in xy:
            z = offset+sum(b*v for b, v in zip(beta, x))
            total += max(z, 0)-y*z+math.log1p(math.exp(-abs(z)))
        return total
    beta = [0.0]*dim
    lo, hi = plan["coefficient_bounds"]
    for _ in range(100):
        ps = [c.sigmoid(offset+sum(b*v for b, v in zip(beta, x))) for offset, x, _ in xy]
        gradient = [sum((p-y)*x[j] for (_, x, y), p in zip(xy, ps))+ridge*beta[j] for j in range(dim)]
        hessian = [[sum(max(1e-12, p*(1-p))*x[j]*x[k] for (_, x, _), p in zip(xy, ps))
                    +(ridge if j == k else 0) for k in range(dim)] for j in range(dim)]
        step = c.terminal.solve(hessian, gradient)
        before = objective(beta)
        for k in range(30):
            trial = [max(lo, min(hi, b-s*.5**k)) for b, s in zip(beta, step)]
            if objective(trial) <= before+1e-12:
                break
        else:
            break
        change = max(abs(a-b) for a, b in zip(trial, beta))
        beta = trial
        if change < 1e-10:
            break
    return dict(intercept=beta[0], increment_coefficient=beta[1] if dim == 2 else 0,
                with_increment=with_increment, discrepancy_scale=scale, ridge=ridge,
                training_samples=len(samples), role="diagnostic_not_trading_model")


def offset_forecast(sample, model):
    return c.sigmoid(c.logit(sample["market"])+model["intercept"]+
                     model["increment_coefficient"]*(sample["p"]-sample["market"])/model["discrepancy_scale"])


def incremental_evaluation(dataset, opportunities, prior_folds, days, plan):
    folds, scored = [], []
    lookup = {r["slug"]: r for r in dataset}
    for day in sorted({r["day"] for r in opportunities}):
        train = c.training_for_day(dataset, day)
        if not c.sufficient_training(train):
            raise ValueError("prior_evaluation_day_without_causal_training")
        if len(train) != prior_folds[day]["training_samples"]:
            raise ValueError("training_cohort_drift")
        market = fit_market_offset(train, False, plan)
        increment = fit_market_offset(train, True, plan)
        folds.append(dict(day=day, fit_at_ms=c.day_start(day), training_samples=len(train),
                          maximum_training_known_at_ms=max(r["known_at_ms"] for r in train),
                          maximum_training_window_end_ms=max(r["end_ms"] for r in train),
                          market_control=market, increment_diagnostic=increment))
        for r in opportunities:
            if r["day"] != day:
                continue
            sample = lookup[r["slug"]]
            p0, p1 = offset_forecast(sample, market), offset_forecast(sample, increment)
            if r["side"] == "down":
                p0, p1 = 1-p0, 1-p1
            y = r["y"]
            loss0 = -math.log(max(1e-12, p0 if y else 1-p0))
            loss1 = -math.log(max(1e-12, p1 if y else 1-p1))
            scored.append(dict(r, market_adjusted_probability=p0, increment_probability=p1,
                               brier_increment_minus_market_adjusted=(p1-y)**2-(p0-y)**2,
                               logloss_increment_minus_market_adjusted=loss1-loss0))
    full_folds = [f for f in folds if f["day"] in days]
    fraction = sum(f["increment_diagnostic"]["increment_coefficient"] > 0 for f in full_folds)/len(full_folds) if full_folds else 0
    keys = ["brier_increment_minus_market_adjusted", "logloss_increment_minus_market_adjusted"]
    summary = dict(n=len(scored), complete_days=days, folds=folds,
                   complete_fold_positive_increment_fraction=fraction,
                   positive_increment_folds=sum(f["increment_diagnostic"]["increment_coefficient"] > 0 for f in full_folds),
                   complete_fold_count=len(full_folds),
                   all_observed_mean_differences=means(scored, keys),
                   complete_day_intervals=day_intervals(scored, days, keys, plan),
                   scores={name: c.scoring([(r[field], r["y"]) for r in scored])
                           for name, field in [("logged_market", "m"), ("raw_model", "q"),
                                               ("market_intercept_control", "market_adjusted_probability"),
                                               ("market_plus_model_increment", "increment_probability")]})
    for stage, condition in [("edge", lambda r: r["has_edge"]), ("funded", lambda r: r["funded"])]:
        subset = [r for r in scored if condition(r)]
        summary[stage+"_secondary"] = dict(n=len(subset), mean_differences=means(subset, keys),
                                         intervals=day_intervals(subset, days, keys, plan))
    return summary, scored


def accounting(row, trade, probability, policy, tolerance):
    """Exact decomposition at FROZEN filled shares; midpoint is not executable."""
    side, qty = trade["side"], trade["shares"]
    market = row["market"] if side == "up" else 1-row["market"]
    y = row["won"] if side == "up" else 1-row["won"]
    q = probability if side == "up" else 1-probability
    opportunity = best_opportunity(row, probability, policy)
    if opportunity["side"] != side or opportunity["edge"] < policy["min_edge"]:
        raise ValueError("archived_trade_not_original_signal_choice")
    ep = row["episode"]
    quality_error = c.terminal.execution.quality(ep, policy)
    if quality_error is not None:
        raise ValueError("filled_trade_snapshot_timing_invalid/"+quality_error)
    signal_asks = c.terminal.execution.asks(ep["signal"]["books"][side])
    delayed_asks = c.terminal.execution.asks(ep["delayed"]["books"][side])
    if not signal_asks or not delayed_asks or qty <= 0:
        raise ValueError("filled_trade_missing_quotes_or_shares")
    signal_ask, delayed_ask = signal_asks[0][0], delayed_asks[0][0]
    raw_cost = trade["cost"]/(1+policy["slippage_bps"]/10000)
    components = dict(gross_excess_over_signal_mid=qty*(y-market),
                      signal_spread_cost=qty*(signal_ask-market),
                      quote_move_cost=qty*(delayed_ask-signal_ask),
                      depth_cost=raw_cost-qty*delayed_ask,
                      slippage_cost=trade["cost"]-raw_cost,
                      fees=trade["fees"], net_pnl=trade["settlement_pnl"],
                      model_expected_net=qty*q-trade["cost"]-trade["fees"],
                      realized_probability_shortfall=qty*(q-y),
                      claimed_excess_over_signal_mid=qty*(q-market))
    cost_keys = ["signal_spread_cost", "quote_move_cost", "depth_cost", "slippage_cost", "fees"]
    error1 = components["gross_excess_over_signal_mid"]-sum(components[k] for k in cost_keys)-components["net_pnl"]
    error2 = components["model_expected_net"]-components["realized_probability_shortfall"]-components["net_pnl"]
    error3 = components["claimed_excess_over_signal_mid"]-sum(components[k] for k in cost_keys)-components["model_expected_net"]
    if max(abs(error1), abs(error2), abs(error3)) > tolerance:
        raise ValueError("cashflow_identity_failed/"+row["slug"])
    if abs(q-trade["predicted_probability"]) > tolerance:
        raise ValueError("frozen_filled_probability_changed")
    if "expected_pnl" in trade and abs(components["model_expected_net"]-trade["expected_pnl"]) > tolerance:
        raise ValueError("archived_expected_pnl_changed")
    signal_at, delayed_at = ep["signal"]["sampled_at_ms"], ep["delayed"]["sampled_at_ms"]
    signal_book, delayed_book = ep["signal"]["books"][side], ep["delayed"]["books"][side]
    if not c.terminal.execution.book_valid(signal_book, signal_at, policy) or not c.terminal.execution.book_valid(delayed_book, delayed_at, policy):
        raise ValueError("filled_trade_book_freshness_invalid")
    return dict(slug=row["slug"], day=c.utc_day(row["time"]), side=side, shares=qty,
                signal_probability_selected=q, signal_midpoint_selected=market, outcome_selected=y,
                signal_raw_ask=signal_ask, delayed_raw_best_ask=delayed_ask,
                signal_at_ms=signal_at, delayed_at_ms=delayed_at, actual_delay_ms=delayed_at-signal_at,
                signal_book_age_ms=signal_at-c.terminal.number(signal_book["http"]["exchange_timestamp"]),
                delayed_book_age_ms=delayed_at-c.terminal.number(delayed_book["http"]["exchange_timestamp"]),
                **components, cashflow_identity_error=error1, expected_identity_error=error2,
                claimed_identity_error=error3)


def accounting_summary(rows, days, tolerance):
    keys = ["gross_excess_over_signal_mid", "signal_spread_cost", "quote_move_cost", "depth_cost",
            "slippage_cost", "fees", "net_pnl", "model_expected_net", "realized_probability_shortfall",
            "claimed_excess_over_signal_mid"]
    totals = {k: sum(r[k] for r in rows) for k in keys}
    execution_keys = ["signal_spread_cost", "quote_move_cost", "depth_cost", "slippage_cost", "fees"]
    aggregate_errors = dict(
        gross_minus_execution_minus_net=totals["gross_excess_over_signal_mid"]-
            sum(totals[k] for k in execution_keys)-totals["net_pnl"],
        expected_minus_shortfall_minus_net=totals["model_expected_net"]-
            totals["realized_probability_shortfall"]-totals["net_pnl"],
        claimed_minus_execution_minus_expected=totals["claimed_excess_over_signal_mid"]-
            sum(totals[k] for k in execution_keys)-totals["model_expected_net"])
    errors = [abs(r[k]) for r in rows for k in
              ("cashflow_identity_error", "expected_identity_error", "claimed_identity_error")]
    errors.extend(abs(v) for v in aggregate_errors.values())
    improvements = sorted((-r["quote_move_cost"] for r in rows if r["quote_move_cost"] < -tolerance), reverse=True)
    full_daily = [sum(r["net_pnl"] for r in rows if r["day"] == day) for day in days]
    return dict(n=len(rows), totals=totals, maximum_identity_error=max(errors, default=0),
                aggregate_identity_errors=aggregate_errors,
                identities_verified=max(errors, default=0) <= tolerance,
                net_execution_drag_vs_signal_mid=sum(totals[k] for k in execution_keys),
                quote_movement_summary=dict(
                    favorable_count=sum(r["quote_move_cost"] < -tolerance for r in rows),
                    adverse_count=sum(r["quote_move_cost"] > tolerance for r in rows),
                    unchanged_count=sum(abs(r["quote_move_cost"]) <= tolerance for r in rows),
                    raw_ask_drop_at_least_half_count=sum(r["delayed_raw_best_ask"] <= .5*r["signal_raw_ask"] for r in rows),
                    total_favorable_component=sum(improvements),
                    largest_favorable_component=improvements[0] if improvements else 0,
                    top_three_favorable_components=sum(improvements[:3])),
                snapshot_timing=dict(
                    maximum_signal_book_age_ms=max((r["signal_book_age_ms"] for r in rows), default=None),
                    maximum_delayed_book_age_ms=max((r["delayed_book_age_ms"] for r in rows), default=None),
                    minimum_delay_ms=min((r["actual_delay_ms"] for r in rows), default=None),
                    maximum_delay_ms=max((r["actual_delay_ms"] for r in rows), default=None),
                    linked_snapshot_rules_verified=True, actual_exchange_fills_verified=False),
                complete_days=days,
                complete_day_pnl_interval=c.terminal.risk.interval_values(full_daily, 4000),
                complete_day_pnl_interval_seed=9,
                daily={day: {k: sum(r[k] for r in rows if r["day"] == day) for k in keys}
                       for day in sorted({r["day"] for r in rows})},
                caveats=["Midpoint is an accounting reference, not an executable counterfactual trade.",
                         "Shares, chosen direction and accepted trades are held fixed; no strategy reselection.",
                         "Quote movement and spread components are signed; do not clip price improvements.",
                         "Expected-minus-probability-shortfall and gross-minus-costs are separate identities; do not double count.",
                         "Favorable delayed snapshots are simulated price improvement, not evidence of actual exchange fills."])


def decision(increment, costs, raw_stress, plan):
    req = plan["decision_requirements"]
    reasons = []
    if increment["n"] < req["minimum_evaluation_windows"]:
        reasons.append("insufficient_common_evaluation_windows")
    if len(increment["complete_days"]) < req["minimum_complete_days"]:
        reasons.append("insufficient_complete_evaluation_days")
    ci = increment["complete_day_intervals"]
    bound = ci.get("metrics", {}).get("brier_increment_minus_market_adjusted", {})
    if not bound.get("available") or bound["upper"] >= req["paired_brier_increment_minus_market_interval_upper_below"]:
        reasons.append("incremental_brier_improvement_interval_not_negative")
    if increment["complete_fold_positive_increment_fraction"] < req["minimum_positive_increment_fold_fraction"]:
        reasons.append("model_disagreement_not_stably_positive")
    if (req["increment_log_loss_no_worse_than_market"] and
            increment["all_observed_mean_differences"]["logloss_increment_minus_market_adjusted"] > 0):
        reasons.append("incremental_log_loss_worse")
    if reasons:
        status = "stop_current_positive_model_edge_trade_route"
    elif costs["totals"]["net_pnl"] <= 0 or raw_stress["pnl"] <= 0:
        status = "investigate_execution_before_new_calibration_candidate"
    else:
        status = "eligible_for_small_conditional_calibration_research_not_deployment"
    return dict(status=status, positive_increment_supported=not reasons, reasons=reasons,
                trading_accepted=False, launch_new_observation=False, live_changes=False,
                inference_scope="reviewed development data; failure is not proof of zero information in all regimes",
                negative_coefficient_policy="diagnostic only; do not automatically reverse a signal")


def verify_prior(folder, plan):
    manifest_path = folder/"manifest.json"
    if c.sha(manifest_path) != plan["calibration_manifest_sha256"]:
        raise ValueError("prior_calibration_manifest_changed")
    manifest = json.loads(manifest_path.read_text())
    for name, digest in manifest["outputs_sha256"].items():
        if c.sha(folder/name) != digest:
            raise ValueError("prior_artifact_changed/"+name)
    for path, digest in manifest["code_sha256"].items():
        if c.sha(path) != digest:
            raise ValueError("prior_calibration_code_changed/"+path)
    for item in manifest["inputs"]:
        if c.sha(Path(item["path"]).parent/"reference-audit.json") != item["labels_sha256"]:
            raise ValueError("prior_labels_changed")
    if c.sha(manifest["base_model"]["path"]) != manifest["base_model"]["sha256"]:
        raise ValueError("base_model_changed")
    cohort = manifest["fixed_cohort"]
    if c.sha(cohort["path"]) != cohort["sha256"]:
        raise ValueError("archived_diagnostic_changed")
    return manifest


def load_episodes(manifest, wanted):
    episodes = {}
    for item in manifest["inputs"]:
        digest = hashlib.sha256()
        with Path(item["path"]).open("rb") as stream:
            for line in stream:
                digest.update(line)
                if b'"residual_execution_observation"' not in line:
                    continue
                e = json.loads(line)
                if e.get("event") == "residual_execution_observation" and e["slug"] in wanted:
                    if e["slug"] in episodes:
                        raise ValueError("duplicate_source_episode")
                    episodes[e["slug"]] = e
        if digest.hexdigest() != item["sha256"]:
            raise ValueError("prior_source_journal_changed")
    if set(episodes) != wanted:
        raise ValueError("source_episode_missing")
    return episodes


def assemble(dataset, forecasts, prior_trades, archived, episodes, policy, plan):
    rows, opportunities = {}, []
    funded = {t["slug"]: t for t in prior_trades if t["method"] == "identity" and t["scenario"] == "base"}
    for r in dataset:
        if r["slug"] not in episodes:
            continue
        ep = episodes[r["slug"]]
        logged = ep["signal"]["terminal_model"]
        p = logged["candidates"][c.BASE_CANDIDATE]["probability_up"]
        if logged["sha256"] != c.BASE_SHA or abs(p-r["p"]) > 1e-12:
            raise ValueError("source_forecast_projection_changed")
        row = dict(slug=r["slug"], asset="eth", time=r["time"], won=r["y"], market=r["market"], episode=ep)
        rows[r["slug"]] = row
        if r["slug"] not in forecasts or forecasts[r["slug"]]["probabilities"]["identity"] is None:
            continue
        if c.signal_eligibility(row, policy) is not None or c.market_eligibility(row, policy) is not None:
            raise ValueError("shared_signal_eligibility_drift")
        opportunity = best_opportunity(row, r["p"], policy)
        m, y = (r["market"], r["y"]) if opportunity["side"] == "up" else (1-r["market"], 1-r["y"])
        q = opportunity["q"]
        has_edge = opportunity["edge"] >= policy["min_edge"]
        if r["slug"] in funded:
            if not has_edge or funded[r["slug"]]["side"] != opportunity["side"]:
                raise ValueError("raw_funded_direction_drift")
            if r["slug"] not in archived:
                raise ValueError("raw_funded_not_in_archived_diagnostic")
        opportunities.append(dict(slug=r["slug"], day=r["day"], **opportunity, m=m, y=y,
                                  claimed_advantage=q-m, realized_advantage=y-m, optimism=q-y,
                                  brier_minus_market=(q-y)**2-(m-y)**2, has_edge=has_edge,
                                  funded=r["slug"] in funded,
                                  disagreement_bin=disagreement_bin(q-m, plan["disagreement_bin_edges"])))
    expected = plan["expected_counts"]
    counts = dict(evaluation_windows=len(opportunities), raw_edge_signals=sum(r["has_edge"] for r in opportunities),
                  raw_funded_fills=sum(r["funded"] for r in opportunities), original_diagnostic_fills=len(archived))
    if counts != expected:
        raise ValueError("frozen_stage_counts_not_reproduced/"+str(counts))
    return rows, opportunities, funded, counts


def study(folder, plan):
    manifest = verify_prior(folder, plan)
    prior = json.loads((folder/"report.json").read_text())
    dataset = [json.loads(l) for l in (folder/"dataset.ndjson").open()]
    forecasts = {e["slug"]: e for e in (json.loads(l) for l in (folder/"forecasts.ndjson").open())}
    all_trades = [json.loads(l) for l in (folder/"trades.ndjson").open()]
    archived = {e["slug"]: e for e in (json.loads(l) for l in Path(manifest["fixed_cohort"]["path"]).open())
                if e["result"]["status"] == "filled"}
    target = {slug for slug, r in forecasts.items() if r["probabilities"]["identity"] is not None} | set(archived)
    episodes = load_episodes(manifest, target)
    policy = prior["fixed_policy"]
    rows, opportunities, funded, counts = assemble(dataset, forecasts, all_trades, archived, episodes, policy, plan)
    days = prior["complete_evaluation_days"]
    groups = dict(eligible=opportunities, edge=[r for r in opportunities if r["has_edge"]],
                  funded=[r for r in opportunities if r["funded"]],
                  no_edge=[r for r in opportunities if not r["has_edge"]],
                  edge_unfilled=[r for r in opportunities if r["has_edge"] and not r["funded"]])
    stages = {k: stage_summary(v, days, plan) for k, v in groups.items()}
    contrasts = dict(edge_minus_no_edge=stage_contrast(groups["edge"], groups["no_edge"], days, plan),
                     funded_minus_edge_unfilled=stage_contrast(groups["funded"], groups["edge_unfilled"], days, plan))
    bins = {key: stage_summary([r for r in opportunities if r["disagreement_bin"] == key], days, plan)
            for key in ("lt_010", "010_025", "ge_025")}
    for value in bins.values():
        value["sufficient_for_descriptive_review"] = value["n"] >= 30
    increment, increment_rows = incremental_evaluation(dataset, opportunities,
        {f["day"]: f for f in prior["folds"]}, days, plan)
    tol = plan["accounting_tolerance"]
    lookup = {r["slug"]: r for r in dataset}
    accounting_primary = [accounting(rows[s], t, lookup[s]["p"], policy, tol) for s, t in funded.items()]
    accounting_full = [accounting(rows[s], e["result"], lookup[s]["p"], policy, tol) for s, e in archived.items()]
    full_days = [day for day in c.complete_days(dataset) if min(r["day"] for r in accounting_full) <= day
                 <= max(r["day"] for r in accounting_full)]
    costs = dict(rolling_114=accounting_summary(accounting_primary, days, tol),
                 full_diagnostic_143=accounting_summary(accounting_full, full_days, tol),
                 raw_base_replay=prior["strategies"]["identity"],
                 stress_note="Stress reruns selection/quantity; it is not the fixed-share accounting identity.")
    if not all(costs[k]["identities_verified"] for k in ("rolling_114", "full_diagnostic_143")):
        raise ValueError("aggregate_cashflow_identity_failed")
    if not math.isclose(costs["rolling_114"]["totals"]["net_pnl"], prior["strategies"]["identity"]["pnl"], abs_tol=tol):
        raise ValueError("accounting_does_not_reconcile_to_prior_pnl")
    result = decision(increment, costs["rolling_114"], prior["strategies"]["identity"]["stress"], plan)
    report = dict(scope=plan["scope"], fixed_counts=counts, complete_evaluation_days=days,
                  stages=stages, stage_contrasts=contrasts, fixed_disagreement_bins=bins,
                  incremental_information=increment, execution_accounting=costs, decision=result,
                  tasks={"T1": dict(status="complete", verified_counts=counts),
                         "T2": dict(status="complete", positive_increment_supported=result["positive_increment_supported"]),
                         "T3": dict(status="complete", identities_verified=all(costs[k]["identities_verified"]
                                                                                 for k in ("rolling_114", "full_diagnostic_143"))),
                         "T4": dict(status="complete", decision=result["status"])},
                  limits=["All data already reviewed; no new independent holdout.",
                          "Nested stage differences describe conditional composition, not causal selection effects.",
                          "One pre-outcome side per market; rows and nested stages are not independent replications.",
                          "Only seven complete rolling-evaluation UTC days; partial final day remains in totals.",
                          "No feature search, threshold optimization, automatic reversal or launch."])
    verification = dict(prior_artifact_hashes_verified=True, prior_source_and_label_hashes_verified=True,
                        frozen_plan_counts_verified=True, prior_calibration_code_unchanged=True,
                        original_direction_preserved=True, one_observation_per_evaluation_market=True,
                        cashflow_identities_verified=True, original_diagnostic_cohort_preserved=True,
                        no_new_calibration_candidate_fitted=True, no_live_changes=True,
                        linked_book_timing_verified=True, actual_exchange_fills_verified=False)
    return report, opportunities, increment_rows, accounting_primary, accounting_full, verification


def markdown(report):
    lines = ["# ETH 增量信息与选单偏差诊断", "", "固定计划下的已查看开发资料诊断，无线上改动。", "",
             "## T1：相同预选合约方向的阶段比较", "",
             "每个窗口只选一个信号时最优方向，再依次施加 edge 和成交条件。", "",
             "| 阶段 | N | 预测概率 | 市场概率 | 胜率 | 声称优势 | 实际相对市场 |",
             "|---|---:|---:|---:|---:|---:|---:|"]
    for key in ("eligible", "edge", "funded", "no_edge", "edge_unfilled"):
        s = report["stages"][key]
        m = s["means"]
        lines.append(f"| {key} | {s['n']} | {m['q']:.2%} | {m['m']:.2%} | {m['y']:.2%} | "
                     f"{m['claimed_advantage']:+.2%} | {m['realized_advantage']:+.2%} |")
    inc = report["incremental_information"]
    lines += ["", "## T2：市场对照与单一模型差异", "",
              "市场对照 logit(m)+截距；增量诊断额外加入 (p−m)/0.10。正负系数均允许，负系数不授权反向交易。", "",
              "| 对照 | Brier | log loss |", "|---|---:|---:|"]
    for name, score in inc["scores"].items():
        lines.append(f"| {name} | {score['brier']:.6f} | {score['log_loss']:.6f} |")
    lines += ["", f"完整日中正增量系数比例 {inc['complete_fold_positive_increment_fraction']:.1%}。",
              f"配对评分差：{inc['all_observed_mean_differences']}。",
              f"完整日区间：{inc['complete_day_intervals']}。", "",
              "## T3：冻结份额的现金流分解", "",
              "净盈亏 = 市场相对实现收益 − 信号价差 − 报价变化 − 深度 − 滑点 − 费用。",
              "另一恒等式：净盈亏 = 模型预测净收益 − 实现概率短差。两式不可重复加总。", "",
              "| 队列 | N | 市场相对收益 | 价差 | 报价变化 | 深度 | 滑点 | 费用 | 净盈亏 |",
              "|---|---:|---:|---:|---:|---:|---:|---:|---:|"]
    for name in ("rolling_114", "full_diagnostic_143"):
        value = report["execution_accounting"][name]
        t = value["totals"]
        lines.append(f"| {name} | {value['n']} | {t['gross_excess_over_signal_mid']:+.2f} | "
                     f"{t['signal_spread_cost']:+.2f} | {t['quote_move_cost']:+.2f} | {t['depth_cost']:+.2f} | "
                     f"{t['slippage_cost']:+.2f} | {t['fees']:.2f} | {t['net_pnl']:+.2f} |")
    lines += ["", "中间价只是记账参考，不能当作可成交买价；价差和报价变化保留符号。",
              "冻结份额记账不构成去掉成本后重新选单的策略，也不提供可交易的反事实收益。", "",
              "## T4：研究决策", "", str(report["decision"]), "",
              "- [固定计划](protocol.json)", "- [全部指标与区间](report.json)",
              "- [逐窗口机会](opportunities.ndjson)", "- [增量滚动预测](incremental-forecasts.ndjson)",
              "- [114 笔现金流](accounting-rolling.ndjson)", "- [143 笔稳健性队列](accounting-full-diagnostic.ndjson)",
              "- [核对记录](verification.json)", "- [哈希清单](manifest.json)", ""]
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plan", default=str(c.ROOT/"scripts/digital_fair_value_incremental_013_plan.json"))
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    output = Path(args.output)
    if output.exists():
        parser.error("choose a NEW output directory")
    plan_path = Path(args.plan)
    original_plan_sha = c.sha(plan_path)
    plan = json.loads(plan_path.read_text())
    folder = c.ROOT/plan["calibration_dir"]
    output.mkdir(parents=True)
    (output/"protocol.json").write_bytes(plan_path.read_bytes())
    report, opportunities, inc_rows, primary, full, verification = study(folder, plan)
    if c.sha(plan_path) != original_plan_sha:
        raise ValueError("research_plan_changed_during_analysis")
    # Recheck small development artifacts after calculation; the journal itself
    # was hashed while loading its selected episodes.
    verify_prior(folder, plan)
    write_json(output/"report.json", report)
    write_json(output/"verification.json", verification)
    for name, rows in [("opportunities.ndjson", opportunities), ("incremental-forecasts.ndjson", inc_rows),
                       ("accounting-rolling.ndjson", primary), ("accounting-full-diagnostic.ndjson", full)]:
        (output/name).write_text("".join(json.dumps(r, sort_keys=True, allow_nan=False)+"\n" for r in rows))
    (output/"analysis.zh-CN.md").write_text(markdown(report))
    dependencies = [Path(__file__), Path(c.__file__), Path(c.terminal.__file__),
                    Path(c.terminal.execution.__file__), Path(c.terminal.risk.__file__)]
    write_json(output/"manifest.json", dict(created_at_utc=dt.datetime.now(dt.timezone.utc).isoformat(),
        plan=dict(path=str(plan_path), sha256=original_plan_sha),
        prior_calibration_manifest=dict(path=str(folder/"manifest.json"), sha256=plan["calibration_manifest_sha256"]),
        code_sha256={str(p): c.sha(p) for p in dependencies},
        outputs_sha256={p.name: c.sha(p) for p in output.iterdir() if p.is_file()},
        reviewed_development_only=True, no_online_promotion=True))
    print(json.dumps(dict(output=str(output), counts=report["fixed_counts"], decision=report["decision"]), indent=2))


if __name__ == "__main__":
    main()
