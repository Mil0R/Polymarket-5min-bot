#!/usr/bin/env python3
"""Audit the independent, funded ETH .97 vs .98 PAPER journals (stdlib only)."""
import argparse
import hashlib
import json
from collections import Counter
from datetime import datetime, timedelta, timezone
from decimal import Decimal
from pathlib import Path

ARMS = ("control_096_098", "eth_cap_097")
ZERO = Decimal(0)
BJT = timezone(timedelta(hours=8))


def dec(value):
    if isinstance(value, bool) or value is None:
        raise ValueError("missing/invalid monetary value")
    result = Decimal(str(value))
    if not result.is_finite():
        raise ValueError("nonfinite monetary value")
    return result


def dt(value):
    result = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if result.tzinfo is None:
        raise ValueError("timestamp has no timezone")
    return result


def require(condition, message):
    if not condition:
        raise ValueError(message)


def summarize(positions):
    executions = [p for p in positions if p.get("execution")]
    filled = [p for p in executions if dec(p["execution"]["filled"]) > 0]
    settled = [p for p in filled if p.get("settlement")]
    wins = [p for p in settled if p["settlement"]["won"]]
    cost = sum((dec(p["execution"]["cost"]) for p in settled), ZERO)
    pnl = sum((dec(p["settlement"]["pnl"]) for p in settled), ZERO)
    return {
        "attempts": len(positions), "executed": len(executions),
        "execution_unknown": len(positions) - len(executions),
        "execution_errors": sum(p["execution"]["outcome"] == "error" for p in executions),
        "no_fills": sum(p["execution"]["outcome"] == "no_fill" for p in executions),
        "settled_fills": len(settled), "wins": len(wins), "losses": len(settled) - len(wins),
        "pending_fills": len(filled) - len(settled),
        "pending_cost": str(sum((dec(p["execution"]["cost"]) for p in filled if not p.get("settlement")), ZERO)),
        "unknown_reserved_cash": str(sum((dec(p["attempt"]["reserved_cost"]) for p in positions if not p.get("execution")), ZERO)),
        "cost": str(cost), "pnl": str(pnl), "roi": float(pnl / cost) if cost else None,
        "win_rate": len(wins) / len(settled) if settled else None,
        "settled_fill_days": len({dt(p["attempt"]["evaluation"]["evaluated_at"]).astimezone(BJT).date() for p in settled}),
    }


def analyze(raw):
    require(not raw or raw.endswith(b"\n"), "incomplete journal tail; sync a complete snapshot")
    require(bool(raw), "empty journal: the new study has not started")
    registration = None
    positions, evaluations, market_labels = {}, {}, {}
    arm_market = {}
    cash = {arm: Decimal(100) for arm in ARMS}
    reasons = {arm: Counter() for arm in ARMS}
    runs = []
    for line_no, line in enumerate(raw.splitlines(), 1):
        try:
            event = json.loads(line)
            action = event["action"]
            if action == "registered":
                require(line_no == 1 and registration is None, "duplicate/nonleading registration")
                require(event["version"] == 1 and event["study_id"] == "eth-ask-cap-097-v1", "unsupported study/version")
                registration = event
                c = event["configuration"]
                require(c["paper_only"] is True and c["automatic_promotion"] is False
                        and dec(c["initial_cash_per_arm"]) == 100 and dec(c["max_order_usdc"]) == 5,
                        "invalid frozen PAPER configuration")
                require(c["arms"] == {ARMS[0]: {"btc_max_ask": "0.98", "eth_max_ask": "0.98"},
                                      ARMS[1]: {"btc_max_ask": "0.98", "eth_max_ask": "0.97"}}, "invalid frozen arm definitions")
                require(event["forward_window_start_ms"] % 300000 == 0, "unaligned forward boundary")
                require(event["forward_window_start_ms"] > dt(event["registered_at"]).timestamp() * 1000, "forward boundary predates registration")
                continue
            require(registration is not None, "missing study registration")
            if action == "run_started":
                runs.append(event)
            elif action == "observed":
                e = event["evaluation"]
                require(e["evaluation_id"] not in evaluations, "duplicate study observation")
                require(e["window_start_ms"] >= registration["forward_window_start_ms"], "historical/partial window in new study")
                require(e["run_id"] in {r["run_id"] for r in runs}, "observation from unregistered run")
                evaluations[e["evaluation_id"]] = e
                require({d["arm"] for d in event["decisions"]} == set(ARMS) and len(event["decisions"]) == 2, "invalid decision arms")
                for d in event["decisions"]:
                    reasons[d["arm"]][d["reason"]] += 1
            elif action == "attempted":
                a = event["attempt"]
                e = a["evaluation"]
                arm, identity = a["arm"], a["attempt_id"]
                require(arm in ARMS and identity not in positions, "invalid/duplicate attempt")
                require(evaluations.get(e["evaluation_id"]) == e, "attempt does not match observation")
                require((arm, e["slug"]) not in arm_market, "duplicate arm/market attempt")
                asset = e["asset"].lower()
                require(asset in ("btc", "eth") and e["slug"] == f'{asset}-updown-5m-{e["window_start_ms"] // 1000}', "inconsistent market identity")
                ask = dec(e["ask_price"])
                require(Decimal(".96") <= ask <= Decimal(".98"), "attempt outside control quote range")
                require(arm != ARMS[1] or asset != "eth" or ask <= Decimal(".97"), "ETH candidate exceeded .97")
                reserve, quantity = dec(a["reserved_cost"]), dec(a["requested"])
                require(quantity > 0 and reserve == quantity * (ask + dec(e["fee_per_share"])) and 0 < reserve <= 5, "invalid fee-inclusive reservation")
                require(cash[arm] >= reserve, "paper cash overdrawn")
                cash[arm] -= reserve
                positions[identity] = {"attempt": a}
                arm_market[arm, e["slug"]] = identity
            elif action == "executed":
                p = positions[event["attempt_id"]]
                require("execution" not in p, "duplicate execution")
                x, a = event["execution"], p["attempt"]
                e = a["evaluation"]
                require(all(x[k] == e[k] for k in ("evaluation_id", "slug", "run_id")), "execution/attempt identity mismatch")
                require(dec(x["requested"]) == dec(a["requested"]) and dec(x["limit_price"]) == dec(e["ask_price"]), "execution request mismatch")
                require(dt(x["simulated_at"]) >= dt(e["evaluated_at"]), "execution predates decision")
                fill, cost = dec(x["filled"]), dec(x["cost"])
                require(0 <= fill <= dec(a["requested"]) and 0 <= cost <= dec(a["reserved_cost"]), "execution exceeded reservation")
                require(x["outcome"] in ("filled", "no_fill", "error"), "unknown execution outcome")
                require((fill > 0 and cost > 0) if x["outcome"] == "filled" else (fill == 0 and cost == 0), "outcome/exposure mismatch")
                require(x["outcome"] != "filled" or dt(x["simulated_at"]) < dt(e["end_date"]), "post-expiry simulated fill")
                cash[a["arm"]] += dec(a["reserved_cost"]) - cost
                p["execution"] = x
            elif action == "settled":
                p = positions[event["attempt_id"]]
                require("execution" in p and "settlement" not in p, "invalid/duplicate settlement")
                a, x = p["attempt"], p["execution"]
                e = a["evaluation"]
                require(type(event["won"]) is bool and type(e["buy_yes"]) is bool, "invalid result boolean")
                require(dt(event["resolved_at"]) >= dt(e["end_date"]), "premature settlement")
                payout = dec(x["filled"]) if event["won"] else ZERO
                require(dec(event["payout"]) == payout and dec(event["pnl"]) == payout - dec(x["cost"]), "invalid settlement amounts")
                yes_won = e["buy_yes"] == event["won"]
                require(market_labels.get(e["slug"], yes_won) == yes_won, "conflicting market settlement labels")
                market_labels[e["slug"]] = yes_won
                cash[a["arm"]] += payout
                p["settlement"] = event
            else:
                raise ValueError(f"unsupported action: {action}")
        except (ValueError, KeyError, TypeError) as error:
            raise ValueError(f"line {line_no}: {error}") from error

    result = {"study_id": registration["study_id"], "registration": registration, "runs": runs,
              "source_sha256": hashlib.sha256(raw).hexdigest(), "source_bytes": len(raw),
              "interpretation": "independent funded PAPER paths; no live orders; not old 006 subset statistics",
              "automatic_promotion": False, "arms": {}, "matched_markets": {}}
    for arm in ARMS:
        rows = [p for p in positions.values() if p["attempt"]["arm"] == arm]
        stats = summarize(rows)
        expected = Decimal(100) + dec(stats["pnl"]) - dec(stats["pending_cost"]) - dec(stats["unknown_reserved_cash"])
        require(cash[arm] == expected, "paper cash conservation failed")
        result["arms"][arm] = {"total": stats, "cash": str(cash[arm]),
            "by_asset": {asset: summarize([p for p in rows if p["attempt"]["evaluation"]["asset"].lower() == asset]) for asset in ("eth", "btc")},
            "decision_counts": dict(reasons[arm])}
    for asset in ("eth", "btc"):
        matched, pending, errors = [], [], []
        slugs = sorted({p["attempt"]["evaluation"]["slug"] for p in positions.values() if p["attempt"]["evaluation"]["asset"].lower() == asset})
        for slug in slugs:
            pair = [positions.get(arm_market.get((arm, slug))) for arm in ARMS]
            if any(p and (not p.get("execution") or not p.get("settlement")) for p in pair):
                pending.append(slug)
                continue
            if any(p and p["execution"]["outcome"] == "error" for p in pair):
                errors.append(slug)
                continue
            pnls = [dec(p["settlement"]["pnl"]) if p else ZERO for p in pair]
            matched.append({"slug": slug, "control_pnl": str(pnls[0]), "candidate_pnl": str(pnls[1]),
                "candidate_minus_control": str(pnls[1] - pnls[0]),
                "control_ask": pair[0]["attempt"]["evaluation"]["ask_price"] if pair[0] else None,
                "candidate_ask": pair[1]["attempt"]["evaluation"]["ask_price"] if pair[1] else None,
                "candidate_later_entry": bool(all(pair) and dt(pair[1]["attempt"]["evaluation"]["evaluated_at"]) > dt(pair[0]["attempt"]["evaluation"]["evaluated_at"]))})
        result["matched_markets"][asset] = {"complete_markets": len(matched), "pending_or_unknown_markets": pending,
            "excluded_execution_error_markets": errors,
            "control_pnl": str(sum((dec(p["control_pnl"]) for p in matched), ZERO)),
            "candidate_pnl": str(sum((dec(p["candidate_pnl"]) for p in matched), ZERO)),
            "candidate_minus_control": str(sum((dec(p["candidate_minus_control"]) for p in matched), ZERO)),
            "later_entry_markets": sum(p["candidate_later_entry"] for p in matched), "details": matched}
    return result


def markdown(report):
    lines = ["# ETH报价上限0.97：独立PAPER对照", "", "每轨道独立100 USDC初始资金，单笔上限5 USDC；主比较为ETH。", "",
             "|轨道/资产|已结算成交|胜/负|净收益 USDC|", "|---|---:|---:|---:|"]
    for arm in ARMS:
        for asset, s in report["arms"][arm]["by_asset"].items():
            lines.append(f'|{arm}/{asset}|{s["settled_fills"]}|{s["wins"]}/{s["losses"]}|{s["pnl"]}|')
        t = report["arms"][arm]["total"]
        lines += ["", f'{arm}：现金{report["arms"][arm]["cash"]}；待结算成交{t["pending_fills"]}，执行未知{t["execution_unknown"]}，执行错误{t["execution_errors"]}，no-fill {t["no_fills"]}。', ""]
    for asset, pair in report["matched_markets"].items():
        lines.append(f'{asset.upper()}完整匹配市场{pair["complete_markets"]}；候选减对照净收益{pair["candidate_minus_control"]}；候选更晚入场市场{pair["later_entry_markets"]}。')
    lines += ["", "未成交不记盈利；执行未知、待结算与错误市场不进入完整匹配收益。原006数据不追认为本轮前瞻。", "结果为模拟，不自动批准实盘或加仓。", ""]
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, default=Path("results/late-resolution-winner-trading/server-low-latency-007-5m/price-cap-study.ndjson"))
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    report = analyze(args.input.read_bytes())
    report["source_path"] = str(args.input)
    output = args.output_dir or args.input.parent / "price-cap-analysis"
    output.mkdir(parents=True, exist_ok=True)
    (output / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    text = markdown(report)
    (output / "report.zh-CN.md").write_text(text)
    print(text)


if __name__ == "__main__":
    main()
