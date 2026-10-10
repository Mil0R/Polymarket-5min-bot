#!/usr/bin/env python3
"""Frozen-rule, offline subset study of recorded main Shadow executions.

Uses only the standard library. Never submits orders or modifies server settings.
Amounts are Decimal; labels are joined by execution.evaluation_id, not by market.
"""

import argparse
import hashlib
import json
import math
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone
from decimal import Decimal, InvalidOperation
from pathlib import Path


UTC = timezone.utc
BEIJING = timezone(timedelta(hours=8))
ROOT = Path(__file__).resolve().parents[1]
DEFAULT_PLAN = ROOT / "scripts/late_resolution_winner_combination_plan_2026-10-05.json"
DEFAULT_OUTPUT = ROOT / "results/late-resolution-winner-trading/combination-study-2026-10-05"
DEFAULT_INPUTS = [
    "results/late-resolution-winner-trading/server-low-latency-005-5m/evaluations.ndjson",
    "results/late-resolution-winner-trading/server-low-latency-006-5m/evaluations.ndjson",
]
FORWARD_RUN = "6bee8cdf-0766-40f5-98e4-e0f3825c7545"


def compare(field, operator, value):
    return {"field": field, "op": operator, "value": str(value)}


def candidate_rules():
    high_price = compare("ask", "ge", ".97")
    low_probability = compare("probability", "lt", ".70")
    return [
        {"id": "control", "label": "现有主研究轨道全部尝试", "condition": {"constant": True}},
        {"id": "probability_070", "label": "概率 ≥0.70", "condition": compare("probability", "ge", ".70")},
        {"id": "last_20s", "label": "剩余时间 ≤20 秒", "condition": compare("remaining", "le", "20")},
        {"id": "exclude_high_price_low_probability", "label": "排除 ask≥0.97 且概率<0.70",
         "condition": {"not": {"all": [high_price, low_probability]}}},
        {"id": "exclude_joint_risk", "label": "排除 ask≥0.97、概率<0.70、距离比<0.20 的交集",
         "condition": {"not": {"all": [high_price, low_probability, compare("ratio", "lt", ".20")]}}},
        {"id": "price_tiered_probability_ratio", "label": "按报价分档要求概率和距离比",
         "condition": {"any": [
             {"all": [compare("ask", "le", ".96"), compare("probability", "ge", ".60"), compare("ratio", "ge", ".10")]},
             {"all": [compare("ask", "gt", ".96"), compare("probability", "ge", ".70"), compare("ratio", "ge", ".20")]},
         ]}},
    ]


def date_time(value):
    result = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if result.tzinfo is None:
        raise ValueError(f"timestamp has no timezone: {value}")
    return result.astimezone(UTC)


def iso(value):
    return value.astimezone(UTC).isoformat(timespec="seconds").replace("+00:00", "Z")


def money(value):
    if isinstance(value, bool) or value is None:
        raise ValueError(f"invalid amount: {value!r}")
    try:
        result = Decimal(str(value))
    except InvalidOperation as error:
        raise ValueError(f"invalid amount: {value!r}") from error
    if not result.is_finite():
        raise ValueError(f"non-finite amount: {value!r}")
    return result


def number(value):
    try:
        return money(value)
    except ValueError:
        return None


def string(value):
    return format(value, "f")


def add_unique(table, key, value, kind):
    if key in table and table[key] != value:
        raise ValueError(f"conflicting {kind}: {key}")
    table[key] = value


def parse_journals(sources):
    """sources = [(relative_path, immutable bytes)]; identical records deduplicate."""
    evaluations, resolutions, executions, runs = {}, {}, {}, {}
    counters = Counter()
    for source, raw in sources:
        if raw and not raw.endswith(b"\n"):
            raise ValueError(f"{source}: incomplete last line; sync again after writer flush")
        for line_no, line in enumerate(raw.splitlines(), 1):
            if not line.strip():
                continue
            try:
                event = json.loads(line)
                action = event["action"]
                if action == "evaluated":
                    row = event["evaluation"]
                    add_unique(evaluations, row["evaluation_id"], row, "evaluation")
                elif action == "resolved":
                    if not isinstance(event["won"], bool):
                        raise ValueError("resolution.won must be a boolean")
                    # A repeated resolution with a later observation time is OK;
                    # a different label or market for the same execution is not.
                    label = {"slug": event["slug"], "won": event["won"]}
                    add_unique(resolutions, event["evaluation_id"], label, "resolution")
                elif action == "shadow_executed":
                    row = event["execution"]
                    if row.get("candidate_stage") == "sensitivity_confirmed":
                        counters["sensitivity_events_excluded"] += 1
                        continue
                    key = row["evaluation_id"]
                    if key in executions:
                        counters["duplicate_main_events"] += 1
                        add_unique(executions, key, row, "main execution")
                    else:
                        executions[key] = row
                elif action == "run_started":
                    add_unique(runs, event["run_id"], event, "run metadata")
            except (ValueError, KeyError, TypeError) as error:
                raise ValueError(f"{source}:{line_no}: {error}") from error
    trades = []
    for key, execution in executions.items():
        if key not in evaluations:
            raise ValueError(f"execution missing its evaluation: {key}")
        evaluation = evaluations[key]
        for field in ("slug", "run_id"):
            if evaluation[field] != execution[field]:
                raise ValueError(f"execution/evaluation {field} mismatch: {key}")
        label = resolutions.get(key)
        if label and label["slug"] != execution["slug"]:
            raise ValueError(f"execution/resolution slug mismatch: {key}")
        filled, cost = money(execution["filled"]), money(execution["cost"])
        if filled < 0 or cost < 0:
            raise ValueError(f"negative fill/cost: {key}")
        outcome = execution["outcome"]
        if outcome == "no_fill":
            if filled or cost:
                raise ValueError(f"no_fill has exposure: {key}")
        elif outcome == "filled":
            if filled <= 0 or cost <= 0:
                raise ValueError(f"filled execution without positive fill/cost: {key}")
        else:
            raise ValueError(f"unsupported main outcome: {outcome}")
        asset = evaluation["asset"].lower()
        if asset not in ("btc", "eth"):
            raise ValueError(f"unsupported asset: {asset}")
        window_ms = evaluation["window_start_ms"]
        if not isinstance(window_ms, int) or isinstance(window_ms, bool):
            raise ValueError(f"invalid window_start_ms: {key}")
        window_start = window_ms // 1000
        if window_ms % 1000 or window_start % 300 or execution["slug"] != f"{asset}-updown-5m-{window_start}":
            raise ValueError(f"not a consistent five-minute market: {key}")
        executed_at = date_time(execution["simulated_at"])
        evaluated_at = date_time(evaluation["evaluated_at"])
        if not window_start <= evaluated_at.timestamp() < window_start + 300 or executed_at < evaluated_at:
            raise ValueError(f"execution/evaluation timestamp outside market: {key}")
        trades.append({"evaluation": evaluation, "execution": execution, "asset": asset,
                       "evaluation_id": key, "slug": execution["slug"], "run_id": execution["run_id"],
                       "window_start": window_start, "executed_at": executed_at,
                       "day": executed_at.astimezone(BEIJING).date().isoformat(),
                       "filled": filled, "cost": cost, "won": label["won"] if label else None})
    return {"trades": trades, "evaluations": evaluations, "resolutions": resolutions,
            "runs": runs, "counters": dict(counters)}


def latest_main(trades):
    latest = {}
    for trade in trades:
        previous = latest.get(trade["slug"])
        if previous and previous["executed_at"] == trade["executed_at"] and previous["evaluation_id"] != trade["evaluation_id"]:
            raise ValueError(f"ambiguous simultaneous main executions: {trade['slug']}")
        if previous is None or trade["executed_at"] > previous["executed_at"]:
            latest[trade["slug"]] = trade
    return sorted(latest.values(), key=lambda t: (t["executed_at"], t["slug"]))


def features(trade):
    evaluation = trade["evaluation"]
    ask = number(evaluation.get("ask_price"))
    if ask is not None and not 0 < ask < 1:
        ask = None
    remaining = number(evaluation.get("seconds_remaining"))
    if remaining is not None and not 0 <= remaining <= 300:
        remaining = None
    # Stale/missing reference observations can have a placeholder probability=0.
    # Never read that placeholder as an actual low model probability.
    observed = number(evaluation.get("model_observed_at_ms"))
    failed = evaluation.get("failed_checks") or []
    valid_reference = observed is not None and observed > 0 and "StalePrice" not in failed and evaluation.get("rejection") != "StalePrice"
    probability = number(evaluation.get("model_probability")) if valid_reference else None
    if probability is not None and not 0 <= probability <= 1:
        probability = None
    boundary = number(evaluation.get("reversal_boundary_bps"))
    ratio = number(evaluation.get("reversal_margin_ratio")) if valid_reference and boundary is not None and boundary > 0 else None
    return {"ask": ask, "probability": probability, "ratio": ratio, "remaining": remaining}


def condition_result(condition, values):
    """Three-valued logic: unknown features never silently become a rejection."""
    if "constant" in condition:
        if not isinstance(condition["constant"], bool):
            raise ValueError("rule constant must be boolean")
        return condition["constant"]
    if "not" in condition:
        result = condition_result(condition["not"], values)
        return None if result is None else not result
    for operator in ("all", "any"):
        if operator in condition:
            results = [condition_result(c, values) for c in condition[operator]]
            if operator == "all":
                return False if False in results else (None if None in results else True)
            return True if True in results else (None if None in results else False)
    value = values[condition["field"]]
    threshold = money(condition["value"])
    if value is None:
        return None
    operation = condition["op"]
    if operation == "ge":
        return value >= threshold
    if operation == "gt":
        return value > threshold
    if operation == "le":
        return value <= threshold
    if operation == "lt":
        return value < threshold
    raise ValueError(f"unknown rule operation: {operation}")


def wilson_lower(wins, total):
    if not total:
        return None
    p, z = wins / total, 1.96
    return (p + z*z/(2*total) - z*math.sqrt(p*(1-p)/total + z*z/(4*total*total))) / (1+z*z/total)


def stats(trades, include_daily=True):
    fills = [t for t in trades if t["filled"] > 0]
    settled = [t for t in fills if t["won"] is not None]
    pending = [t for t in fills if t["won"] is None]
    winners = [t for t in settled if t["won"]]
    losers = [t for t in settled if not t["won"]]
    cost = sum((t["cost"] for t in settled), Decimal(0))
    shares = sum((t["filled"] for t in settled), Decimal(0))
    payout = sum((t["filled"] for t in winners), Decimal(0))
    winning_pnl = sum((t["filled"] - t["cost"] for t in winners), Decimal(0))
    losing_pnl = -sum((t["cost"] for t in losers), Decimal(0))
    result = {"attempts": len(trades), "filled_attempts": len(fills), "settled_fills": len(settled),
              "wins": len(winners), "losses": len(losers), "no_fills": len(trades)-len(fills),
              "pending_attempts": sum(t["won"] is None for t in trades), "pending_fills": len(pending),
              "pending_cost": string(sum((t["cost"] for t in pending), Decimal(0))),
              "cost": string(cost), "shares": string(shares), "payout": string(payout),
              "pnl": string(payout-cost), "winning_pnl": string(winning_pnl), "losing_pnl": string(losing_pnl),
              "roi": float((payout-cost)/cost) if cost else None,
              "win_rate": len(winners)/len(settled) if settled else None,
              "wilson_95_lower": wilson_lower(len(winners), len(settled)),
              "break_even": float(cost/shares) if shares else None,
              "settled_fill_days": len({t["day"] for t in settled}),
              "first_attempt_at": iso(min(t["executed_at"] for t in trades)) if trades else None,
              "last_attempt_at": iso(max(t["executed_at"] for t in trades)) if trades else None,
              "loss_markets": [t["slug"] for t in losers]}
    if include_daily:
        daily = defaultdict(list)
        for trade in trades:
            daily[trade["day"]].append(trade)
        result["daily_beijing"] = {day: stats(rows, False) for day, rows in sorted(daily.items())}
    return result


def by_asset(trades, include_daily=True):
    return {asset: stats([t for t in trades if asset == "all" or t["asset"] == asset], include_daily)
            for asset in ("all", "eth", "btc")}


def evaluate_rules(trades, rules):
    result = {}
    for rule in rules:
        groups = {"kept": [], "rejected": [], "unknown": []}
        for trade in trades:
            decision = condition_result(rule["condition"], features(trade))
            group = "unknown" if decision is None else ("kept" if decision else "rejected")
            groups[group].append(trade)
        result[rule["id"]] = {"label": rule["label"], **{group: by_asset(rows) for group, rows in groups.items()}}
    return result


def descriptive_cells(trades):
    """Fixed bins describe overlap; they are not new candidates or a grid search."""
    cells, unknown = defaultdict(list), []
    for trade in trades:
        f = features(trade)
        if any(f[k] is None for k in ("ask", "probability", "ratio")):
            unknown.append(trade)
            continue
        price = "ask<=0.96" if f["ask"] <= Decimal(".96") else ("0.96<ask<0.98" if f["ask"] < Decimal(".98") else "ask>=0.98")
        probability = "p<0.60" if f["probability"] < Decimal(".60") else ("0.60<=p<0.70" if f["probability"] < Decimal(".70") else "p>=0.70")
        ratio = "ratio<0.10" if f["ratio"] < Decimal(".10") else ("0.10<=ratio<0.20" if f["ratio"] < Decimal(".20") else "ratio>=0.20")
        cells[(trade["asset"], price, probability, ratio)].append(trade)
    return {"cells": [{"asset": key[0], "ask_bin": key[1], "probability_bin": key[2], "ratio_bin": key[3],
                       "stats": stats(rows)} for key, rows in sorted(cells.items())], "unknown": by_asset(unknown)}


def phase_report(trades, rules):
    missing = Counter()
    for trade in trades:
        for field, value in features(trade).items():
            if value is None:
                missing[field] += 1
    return {"control": by_asset(trades), "rules": evaluate_rules(trades, rules),
            "missing_feature_attempts": dict(missing), "descriptive_joint_bins": descriptive_cells(trades)}


def source_metadata(source, raw):
    return {"path": source, "bytes": len(raw), "lines": len(raw.splitlines()), "sha256": hashlib.sha256(raw).hexdigest()}


def registration_digest(plan):
    contents = {key: value for key, value in plan.items() if key != "registration_sha256"}
    return hashlib.sha256(json.dumps(contents, sort_keys=True, separators=(",", ":"), ensure_ascii=False,
                                     allow_nan=False).encode("utf-8")).hexdigest()


def snapshot_inputs(root, descriptors):
    frozen, current, provenance = [], [], []
    for descriptor in descriptors:
        source = descriptor["path"]
        raw = (root / source).read_bytes()
        size = descriptor["bytes"]
        if len(raw) < size:
            raise ValueError(f"frozen journal truncated: {source}")
        prefix = raw[:size]
        if hashlib.sha256(prefix).hexdigest() != descriptor["sha256"]:
            raise ValueError(f"frozen journal prefix changed: {source}")
        if len(prefix.splitlines()) != descriptor["lines"]:
            raise ValueError(f"frozen line count mismatch: {source}")
        frozen.append((source, prefix))
        current.append((source, raw))
        provenance.append({**source_metadata(source, raw), "appended_bytes": len(raw)-size,
                           "appended_lines": len(raw.splitlines())-descriptor["lines"]})
    return frozen, current, provenance


def register_plan(root, plan_path, inputs=None, now=None, forward_run_id=FORWARD_RUN):
    if plan_path.exists():
        raise ValueError(f"plan already registered; refusing overwrite: {plan_path}")
    inputs = inputs if inputs is not None else DEFAULT_INPUTS
    sources = [(str(source), (root / source).read_bytes()) for source in inputs]
    frozen = parse_journals(sources)
    baseline = latest_main(frozen["trades"])
    run = frozen["runs"].get(forward_run_id)
    if not run or run.get("mode") != "paper" or run.get("hybrid_paper_enabled") is not True:
        raise ValueError("expected 006 paper/hybrid run is absent; review protocol before registering")
    latest_run = max(frozen["runs"].values(), key=lambda r: date_time(r["started_at"]))
    if latest_run["run_id"] != forward_run_id:
        raise ValueError("latest recorded run differs from registered forward run")
    # Timestamp after the input snapshot and rule definition. Future backfilled
    # rows from the unobserved sync gap must not masquerade as forward samples.
    now = now if now is not None else datetime.now(UTC)
    forward_epoch = (int(now.timestamp()) // 300 + 1) * 300
    plan = {"schema_version": 1, "study_id": "late-winner-combination-2026-10-05",
            "registered_at": iso(now), "forward_window_start": iso(datetime.fromtimestamp(forward_epoch, UTC)),
            "inputs": [source_metadata(source, raw) for source, raw in sources],
            "scope": {"forward_run_id": forward_run_id, "forward_source": str(inputs[-1]), "assets": ["eth", "btc"],
                      "mode": "paper", "hybrid_paper_enabled": True, "market_seconds": 300,
                      "order_budget_usdc": "5", "primary_selection": "latest_main_execution_per_market",
                      "settlement_join": "execution.evaluation_id", "sensitivity_only_excluded": True},
            "rules": candidate_rules(), "baseline_at_registration": by_asset(baseline, False),
            "review_dates_beijing": {"earliest": "2026-10-12", "first_review": "2026-10-19",
                                     "parameter_review_window": ["2026-10-26", "2026-11-09"]},
            "sample_targets": {"new_settled_eth_fills_minimum": 300, "new_settled_eth_fills_preferred": 500,
                               "comparable_eth_losses_minimum": 10, "comparable_eth_losses_preferred": 15,
                               "per_rule_new_settled_eth_fills_minimum": 100,
                               "per_rule_new_settled_eth_fills_preferred": 200, "per_rule_fill_days_minimum": 7},
            "automatic_promotion": False,
            "interpretation": ["Historical candidate selection is retrospective; only new full market windows are forward validation.",
                               "This is subset replay of recorded attempts, not re-entry simulation or account PnL.",
                               "Model probability and margin/boundary ratio are correlated, uncalibrated features.",
                               "Keep server strategy, confirmations, volatility floors and 5 USDC budget unchanged during validation.",
                               "Sample milestones and nominal Wilson intervals never automatically authorize a parameter change."]}
    plan["registration_sha256"] = registration_digest(plan)
    plan_path.parent.mkdir(parents=True, exist_ok=True)
    with plan_path.open("x", encoding="utf-8") as stream:
        json.dump(plan, stream, ensure_ascii=False, indent=2, allow_nan=False)
        stream.write("\n")
    return plan


def analyze_plan(root, plan_path):
    plan_raw = plan_path.read_bytes()
    plan = json.loads(plan_raw)
    if plan.get("registration_sha256") != registration_digest(plan):
        raise ValueError("registered plan checksum changed; do not edit frozen rules or boundaries")
    if plan["schema_version"] != 1 or plan.get("automatic_promotion") is not False:
        raise ValueError("unsupported or automatic-promotion plan")
    frozen_sources, current_sources, provenance = snapshot_inputs(root, plan["inputs"])
    frozen, current = parse_journals(frozen_sources), parse_journals(current_sources)
    baseline = latest_main(frozen["trades"])
    if by_asset(baseline, False) != plan["baseline_at_registration"]:
        raise ValueError("baseline stats differ from registered values; review code/protocol version")
    baseline_ids = {t["evaluation_id"] for t in frozen["trades"]}
    baseline_markets = {e["slug"] for e in frozen["evaluations"].values()}
    current_by_id = {t["evaluation_id"]: t for t in current["trades"]}
    baseline_followup = [current_by_id[t["evaluation_id"]] for t in baseline]
    boundary = date_time(plan["forward_window_start"]).timestamp()
    registered = date_time(plan["registered_at"])
    scope = plan["scope"]
    buckets = {"forward": [], "pre_forward_gap": [], "baseline_market_overlap": [], "unregistered_run": []}
    source_for_execution = {}
    for source, raw in current_sources:
        for event in map(json.loads, (line for line in raw.splitlines() if line.strip())):
            if event["action"] == "shadow_executed" and event["execution"].get("candidate_stage") != "sensitivity_confirmed":
                source_for_execution.setdefault(event["execution"]["evaluation_id"], set()).add(source)
    for trade in current["trades"]:
        if trade["evaluation_id"] in baseline_ids:
            continue
        if trade["slug"] in baseline_markets:
            bucket = "baseline_market_overlap"
        elif trade["window_start"] < boundary or trade["executed_at"] < registered:
            bucket = "pre_forward_gap"
        else:
            run = current["runs"].get(trade["run_id"], {})
            compatible = (trade["run_id"] == scope["forward_run_id"] and run.get("mode") == scope["mode"]
                          and run.get("hybrid_paper_enabled") == scope["hybrid_paper_enabled"]
                          and scope["forward_source"] in source_for_execution[trade["evaluation_id"]])
            bucket = "forward" if compatible else "unregistered_run"
        buckets[bucket].append(trade)
    # Include idle/new runs in deviations even when they have no executions yet.
    new_runs = [r for r in current["runs"].values() if date_time(r["started_at"]) >= registered and r["run_id"] != scope["forward_run_id"]]
    deviations = []
    if new_runs:
        deviations.append("new_run_since_registration")
    if buckets["unregistered_run"]:
        deviations.append("attempts_from_unregistered_run_or_source")
    if buckets["baseline_market_overlap"]:
        deviations.append("new_attempt_in_baseline_market")
    forward = latest_main(buckets["forward"])
    base_report, forward_report = phase_report(baseline, plan["rules"]), phase_report(forward, plan["rules"])
    targets = plan["sample_targets"]
    eth = forward_report["control"]["eth"]
    losses = base_report["control"]["eth"]["losses"] + eth["losses"]
    progress = {"new_settled_eth_fills": eth["settled_fills"], "comparable_eth_losses": losses,
                "baseline_eth_losses": base_report["control"]["eth"]["losses"],
                "forward_eth_losses": eth["losses"],
                "overall_sample_milestones_met": eth["settled_fills"] >= targets["new_settled_eth_fills_minimum"] and losses >= targets["comparable_eth_losses_minimum"],
                "rule_progress": {}}
    for rule in plan["rules"]:
        if rule["id"] == "control":
            continue
        group = forward_report["rules"][rule["id"]]
        kept = group["kept"]["eth"]
        adequate = kept["settled_fills"] >= targets["per_rule_new_settled_eth_fills_minimum"] and kept["settled_fill_days"] >= targets["per_rule_fill_days_minimum"]
        progress["rule_progress"][rule["id"]] = {
            "kept_new_settled_eth_fills": kept["settled_fills"], "kept_fill_days": kept["settled_fill_days"],
            "group_sample_milestones_met": adequate,
            "unknown_attempts": group["unknown"]["eth"]["attempts"],
            "nominal_wilson_above_weighted_break_even": (kept["wilson_95_lower"] > kept["break_even"]) if kept["settled_fills"] else None,
            "status": "sample_review_available" if adequate and progress["overall_sample_milestones_met"] and not deviations and not group["unknown"]["eth"]["attempts"] else "collecting_or_protocol_review_required"}
    return {"schema_version": 1, "study_id": plan["study_id"], "generated_at": iso(datetime.now(UTC)),
            "plan_sha256": hashlib.sha256(plan_raw).hexdigest(), "registered_at": plan["registered_at"],
            "forward_window_start": plan["forward_window_start"], "input_provenance": provenance,
            "review_dates_beijing": plan["review_dates_beijing"], "sample_targets": targets,
            "baseline_frozen": base_report, "baseline_with_later_labels": by_asset(baseline_followup),
            "forward": forward_report, "progress": progress,
            "excluded_new_attempts": {name: by_asset(latest_main(rows)) for name, rows in buckets.items() if name != "forward"},
            "protocol_deviations": deviations, "new_runs_since_registration": new_runs,
            "journal_counters": current["counters"], "automatic_promotion": False,
            "interpretation": plan["interpretation"]}


def percentage(value):
    return "—" if value is None else f"{value*100:.3f}%"


def render_markdown(report):
    registered = date_time(report["registered_at"]).astimezone(BEIJING).strftime("%Y-%m-%d %H:%M:%S")
    forward_start = date_time(report["forward_window_start"]).astimezone(BEIJING).strftime("%Y-%m-%d %H:%M:%S")
    lines = ["# Late Resolution Winner：冻结组合研究", "",
             f"登记时间：北京时间 {registered}；前瞻市场窗口起点：{forward_start}。", "",
             "历史样本用于提出候选。前瞻样本只接收登记后的完整五分钟市场、指定 006 run 的主 Shadow 执行；",
             "登记前同步缺口、旧市场重试和其他 run 单独统计。结算按执行关联 evaluation ID，排除 sensitivity-only。", "",
             "收益是已记录尝试的子集回放，含 journal 中模拟费用，不是新策略可实现收益或实盘账户收益。", ""]
    for phase, label in (("baseline_frozen", "冻结历史基线"), ("forward", "登记后前瞻验证")):
        section = report[phase]
        lines.extend([f"## {label}", "", "|资产|尝试|成交已结算|胜 / 负|未成交|待结算成交|净收益 USDC|ROI|Wilson 95% 下界|成本/份额盈亏平衡|", "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"])
        for asset in ("all", "eth", "btc"):
            s = section["control"][asset]
            lines.append(f"|{asset.upper()}|{s['attempts']}|{s['settled_fills']}|{s['wins']} / {s['losses']}|{s['no_fills']}|{s['pending_fills']}|{s['pnl']}|{percentage(s['roi'])}|{percentage(s['wilson_95_lower'])}|{percentage(s['break_even'])}|")
        for asset in ("eth", "btc"):
            lines.extend(["", f"### {asset.upper()} 固定候选", "", "|候选|保留尝试 / 已结算成交|保留胜 / 负|保留净收益|剔除胜 / 负|剔除净收益|未知特征尝试|", "|---|---:|---:|---:|---:|---:|---:|"])
            for rule_id, group in section["rules"].items():
                if rule_id == "control":
                    continue
                k, r, u = (group[name][asset] for name in ("kept", "rejected", "unknown"))
                lines.append(f"|{group['label']}|{k['attempts']} / {k['settled_fills']}|{k['wins']} / {k['losses']}|{k['pnl']}|{r['wins']} / {r['losses']}|{r['pnl']}|{u['attempts']}|")
        lines.extend(["", "剔除组盈利为正时，意味着过滤也会放弃该净收益。各候选重叠，不能相加。", "",
                      f"缺失特征尝试：`{json.dumps(section['missing_feature_attempts'], ensure_ascii=False)}`。", ""])
    progress = report["progress"]
    targets = report["sample_targets"]
    dates = report["review_dates_beijing"]
    lines.extend(["## 样本进度与复核", "",
                  f"- 新增已结算 ETH 成交：{progress['new_settled_eth_fills']} / 最低 {targets['new_settled_eth_fills_minimum']}，优选 {targets['new_settled_eth_fills_preferred']}。",
                  f"- 可比 ETH 负场：{progress['comparable_eth_losses']} / 最低 {targets['comparable_eth_losses_minimum']}，优选 {targets['comparable_eth_losses_preferred']}（历史 {progress['baseline_eth_losses']}，前瞻 {progress['forward_eth_losses']}）。",
                  f"- 各候选保留组：最低 {targets['per_rule_new_settled_eth_fills_minimum']} 笔，优选 {targets['per_rule_new_settled_eth_fills_preferred']} 笔，至少 {targets['per_rule_fill_days_minimum']} 个有成交的北京时间日期。",
                  f"- 第一次正式复核：{dates['first_review']}；最早观察节点 {dates['earliest']}；参数评审窗口 {' 至 '.join(dates['parameter_review_window'])}。",
                  "- 以上为研究样本节点，不是自动调参或实盘准入条件；达到日期但样本不足就继续积累。", "",
                  "|候选|前瞻 ETH 保留成交|有成交日期|样本节点满足|名义 Wilson 下界超过成本/份额|", "|---|---:|---:|---|---|"])
    for rule_id, p in progress["rule_progress"].items():
        confidence = p["nominal_wilson_above_weighted_break_even"]
        lines.append(f"|{report['forward']['rules'][rule_id]['label']}|{p['kept_new_settled_eth_fills']}|{p['kept_fill_days']}|{'是' if p['group_sample_milestones_met'] else '否'}|{'样本不足' if confidence is None else ('是' if confidence else '否')}|")
    lines.extend(["", "## 数据边界与解释", "",
                  f"协议偏离：`{json.dumps(report['protocol_deviations'], ensure_ascii=False)}`。", "",
                  "|排除原因|新尝试|已结算成交|净收益 USDC|", "|---|---:|---:|---:|"])
    for name, groups in report["excluded_new_attempts"].items():
        s = groups["all"]
        lines.append(f"|{name}|{s['attempts']}|{s['settled_fills']}|{s['pnl']}|")
    lines.extend(["", "概率与距离比来自共同的距离、波动率和时间，具有相关性；0.70 不是已校准的真实 70% 胜率。",
                  "距离比保留方向符号，0.20 表示距离为完整反转边界的 20%，不是已越过边界。",
                  "Wilson 下界是按独立同分布假设计算的名义指标；日内相关性、成本差异与多候选选择均限制其解释。",
                  "JSON 中保存固定报价×概率×距离比分箱、每日统计、保留/剔除/未知的金额与置信指标，未执行最优分箱搜索。", "",
                  f"计划 SHA256：`{report['plan_sha256']}`。输入冻结前缀必须保持 SHA256 一致；修改或截断会报错。", ""])
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    register = commands.add_parser("register", help="register once; refuses existing plan")
    register.add_argument("--plan", type=Path, default=DEFAULT_PLAN)
    register.add_argument("--forward-run-id", default=FORWARD_RUN, help="explicit run for a separately reviewed new protocol")
    analyze = commands.add_parser("analyze", help="validate frozen prefixes and report new markets")
    analyze.add_argument("--plan", type=Path, default=DEFAULT_PLAN)
    analyze.add_argument("--output-dir", type=Path, default=DEFAULT_OUTPUT)
    args = parser.parse_args()
    try:
        if args.command == "register":
            plan = register_plan(ROOT, args.plan, forward_run_id=args.forward_run_id)
            print(f"Registered: {args.plan}")
            print(f"Forward full-window boundary (UTC): {plan['forward_window_start']}")
        else:
            report = analyze_plan(ROOT, args.plan)
            args.output_dir.mkdir(parents=True, exist_ok=True)
            # Complete computation before replacing reports so invalid inputs
            # cannot erase the previous successful analysis.
            for name, contents in (("report.json", json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False)+"\n"),
                                   ("report.zh-CN.md", render_markdown(report))):
                destination = args.output_dir / name
                temporary = destination.with_suffix(destination.suffix + ".tmp")
                temporary.write_text(contents, encoding="utf-8")
                temporary.replace(destination)
            eth = report["forward"]["control"]["eth"]
            print(f"Forward ETH: {eth['settled_fills']} settled fills, {eth['wins']} wins / {eth['losses']} losses; PnL {eth['pnl']} USDC")
            print(f"Report: {args.output_dir / 'report.zh-CN.md'}")
            if report["protocol_deviations"]:
                print("Protocol deviations: " + ", ".join(report["protocol_deviations"]))
    except (ValueError, KeyError, OSError) as error:
        parser.exit(2, f"error: {error}\n")


if __name__ == "__main__":
    main()
