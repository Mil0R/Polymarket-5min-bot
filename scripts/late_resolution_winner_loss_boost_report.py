#!/usr/bin/env python3
"""Audit per-asset loss-triggered PAPER budgets and report PnL by entry regime."""
import argparse
import hashlib
import json
import re
from datetime import datetime, timezone
from decimal import Decimal as D
from pathlib import Path

import late_resolution_winner_price_cap_report as common

STUDY = "winner-loss-boost-1h-v1"
ASSETS = ("btc", "eth")
ARM = "eth_cap_097"
HOUR_NS = 3_600_000_000_000


def ns(value):
    stamp = common.dt(value)
    fraction = re.search(r"\.(\d+)", value)
    tail = int((fraction[1] if fraction else "").ljust(9, "0")[:9])
    return int(stamp.replace(microsecond=0).timestamp()) * 1_000_000_000 + tail


def iso(value):
    seconds, fraction = divmod(value, 1_000_000_000)
    return datetime.fromtimestamp(seconds, timezone.utc).strftime("%Y-%m-%dT%H:%M:%S") + f".{fraction:09d}Z"


def analyze(raw, now=None):
    require = common.require
    dec = common.dec
    require(raw and raw.endswith(b"\n"), "empty/incomplete loss-boost journal")
    events = [json.loads(line) for line in raw.splitlines()]
    reg = events[0]
    require(reg["action"] == "registered" and reg["version"] == 3 and reg["study_id"] == STUDY,
            "not a loss-boost PAPER journal")
    c = reg["configuration"]
    base, capital = dec(c["base_order_usdc"]), dec(c["initial_cash_per_arm"])
    boosted = base * 10
    require(c["study_id"] == STUDY and c["paper_only"] is True and c["automatic_promotion"] is False
            and c["boost_duration_secs"] == 3600 and c["boost_multiplier"] == 10
            and c["arms"] == {ARM: {"btc_max_ask": "0.98", "eth_max_ask": "0.97"}}
            and 1 <= base <= 100 and base.as_tuple().exponent >= -2
            and boosted <= capital <= 1000000 and capital.as_tuple().exponent >= -2
            and dec(c["max_order_usdc"]) == boosted, "invalid loss-boost configuration")
    cash = capital
    last_loss = dict.fromkeys(ASSETS)
    triggers = []
    positions, observed, decisions = {}, {}, {}
    markets, runs = set(), set()
    labels = {}
    cash_skips = dict.fromkeys(ASSETS, 0)

    def cap(asset, at):
        loss = last_loss[asset]
        return boosted if loss is not None and loss <= at < loss + HOUR_NS else base

    for event in events[1:]:
        action = event["action"]
        if action == "run_started":
            require(event["run_id"] not in runs, "duplicate run")
            runs.add(event["run_id"])
        elif action == "observed":
            e = event["evaluation"]
            key = e["evaluation_id"]
            require(key not in observed and e["run_id"] in runs and e["asset"] in ASSETS
                    and e["window_start_ms"] >= reg["forward_window_start_ms"]
                    and len(event["decisions"]) == 1 and event["decisions"][0]["arm"] == ARM,
                    "invalid observation")
            observed[key] = e
            reason = event["decisions"][0]["reason"]
            decisions[key] = reason
            if reason == "insufficient_paper_cash":
                cash_skips[e["asset"]] += 1
        elif action == "attempted":
            a = event["attempt"]
            e = a["evaluation"]
            key, asset = a["attempt_id"], e["asset"]
            e_id = e["evaluation_id"]
            require(key not in positions and e == observed.get(e_id) and decisions.get(e_id) == "attempt"
                    and a["arm"] == ARM and e["slug"] not in markets, "invalid/duplicate attempt")
            limit = cap(asset, ns(e["evaluated_at"]))
            ask, fee = dec(e["ask_price"]), dec(e["fee_per_share"])
            quantity, reserve = dec(a["requested"]), dec(a["reserved_cost"])
            require(dec(a["budget_limit"]) == limit and a.get("entry_epoch") is None
                    and a.get("parameter_revision") is None and D(".96") <= ask <=
                    (D(".97") if asset == "eth" else D(".98")) and quantity > 0
                    and reserve == quantity * (ask + fee) and 0 < reserve <= min(limit, cash),
                    "attempt violates reconstructed asset budget or cash")
            cash -= reserve
            markets.add(e["slug"])
            positions[key] = {"attempt": a, "regime": "boosted" if limit == boosted else "base"}
        elif action == "executed":
            p = positions[event["attempt_id"]]
            a, x = p["attempt"], event["execution"]
            e = a["evaluation"]
            fill, cost = dec(x["filled"]), dec(x["cost"])
            require("execution" not in p and all(x[k] == e[k] for k in ("run_id", "slug", "evaluation_id"))
                    and dec(x["requested"]) == dec(a["requested"]) and dec(x["limit_price"]) == dec(e["ask_price"])
                    and 0 <= fill <= dec(a["requested"]) and 0 <= cost <= dec(a["reserved_cost"])
                    and ns(x["simulated_at"]) >= ns(e["evaluated_at"]), "invalid execution")
            if x["outcome"] == "filled":
                require(fill > 0 and cost > 0 and ns(x["simulated_at"]) < ns(e["end_date"]),
                        "invalid filled execution")
            else:
                require(x["outcome"] in ("no_fill", "error") and fill == cost == 0, "invalid no-fill")
            p["execution"] = x
            cash += dec(a["reserved_cost"]) - cost
        elif action == "settled":
            p = positions[event["attempt_id"]]
            e, x = p["attempt"]["evaluation"], p["execution"]
            confirmed = ns(event["resolved_at"])
            require("settlement" not in p and type(event["won"]) is bool
                    and confirmed >= ns(e["end_date"]), "invalid settlement")
            payout = dec(x["filled"]) if event["won"] else D(0)
            pnl = payout - dec(x["cost"])
            require(dec(event["payout"]) == payout and dec(event["pnl"]) == pnl, "invalid PnL")
            up_won = e["buy_yes"] == event["won"]
            require(labels.setdefault(e["slug"], up_won) == up_won, "conflicting market labels")
            p["settlement"] = event
            cash += payout
            if pnl < 0:
                asset = e["asset"]
                last_loss[asset] = max(last_loss[asset] or confirmed, confirmed)
                triggers.append(dict(asset=asset, slug=e["slug"], attempt_id=event["attempt_id"],
                                     pnl=str(pnl), confirmed_at=event["resolved_at"],
                                     boost_until=iso(last_loss[asset] + HOUR_NS)))
        else:
            raise ValueError("unknown event: " + action)

    rows = list(positions.values())
    total = common.summarize(rows)
    require(cash == capital + dec(total["pnl"]) - dec(total["pending_cost"])
            - dec(total["unknown_reserved_cash"]), "cash conservation failed")
    as_of = ns((now or datetime.now(timezone.utc)).isoformat())
    assets = {}
    for asset in ASSETS:
        subset = [p for p in rows if p["attempt"]["evaluation"]["asset"] == asset]
        loss = last_loss[asset]
        current = cap(asset, as_of)
        assets[asset] = dict(current_budget=str(current),
                             boost_until=iso(loss + HOUR_NS) if current == boosted else None,
                             insufficient_cash_observations=cash_skips[asset],
                             total=common.summarize(subset),
                             by_regime={mode: common.summarize([p for p in subset if p["regime"] == mode])
                                        for mode in ("base", "boosted")})
    return dict(study_id=STUDY, source_sha256=hashlib.sha256(raw).hexdigest(), as_of=iso(as_of),
                forward_window_start_ms=reg["forward_window_start_ms"], base_budget=str(base),
                boosted_budget=str(boosted), capital=str(capital), cash=str(cash), run_count=len(runs),
                total=total, by_asset=assets, loss_triggers=triggers)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--input", required=True, type=Path)
    args = p.parse_args()
    print(json.dumps(analyze(args.input.read_bytes()), ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
