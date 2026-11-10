#!/usr/bin/env python3
"""Audit the separate, adjustable Operator PAPER ledger, grouped by parameter revision."""
import argparse
import hashlib
import json
from pathlib import Path
from decimal import Decimal as D
import late_resolution_winner_price_cap_report as common


def analyze(raw):
    common.require(raw and raw.endswith(b"\n"), "empty/incomplete operator journal")
    events = [json.loads(line) for line in raw.splitlines()]
    registered = events[0]
    common.require(registered.get("action") == "registered" and registered["version"] == 2
                   and registered["study_id"] == "winner-operator-paper-v2", "not an Operator PAPER v2 journal")
    config = registered["configuration"]
    common.require(config["paper_only"] is True and config["automatic_promotion"] is False
                   and config["initial_cash_per_arm"] == "100" and config["max_order_usdc"] == "100"
                   and config["initial_budget"] == "5" and config["initial_paused"] is True
                   and config["arms"] == {"eth_cap_097": {"btc_max_ask": "0.98", "eth_max_ask": "0.97"}}, "invalid operator configuration")
    settings = dict(revision=0, entry_epoch=0, paused=True, budget="5", pending_budget=None, effective_window_ms=None)
    cash, positions, observations, runs, commands, markets, labels = D(100), {}, {}, set(), set(), set(), {}

    deposits, funding_events = D(0), []

    def budget(v):
        value = common.dec(v)
        common.require(1 <= value <= 100 and value.as_tuple().exponent >= -2, "invalid operator budget")
        return value

    def funding_amount(v):
        value = common.dec(v)
        common.require(0 < value <= 1000000 and value.as_tuple().exponent >= -2, "invalid PAPER deposit")
        return value

    for event in events[1:]:
        action = event["action"]
        if action == "run_started":
            common.require(event["run_id"] not in runs, "duplicate run")
            runs.add(event["run_id"])
        elif action == "operator_funded":
            c = event["command"]; amount = funding_amount(event["amount"])
            common.dt(event["funded_at"])
            common.require(c["id"] not in commands and c["run_id"] in runs
                           and c["expected_revision"] == settings["revision"]
                           and event["revision"] == settings["revision"]+1
                           and c["action"]["action"] == "deposit"
                           and funding_amount(c["action"]["amount"]) == amount, "invalid/duplicate funding command")
            commands.add(c["id"])
            cash += amount; deposits += amount
            settings = dict(settings, revision=event["revision"])
            funding_events.append(dict(command_id=c["id"], amount=str(amount), revision=event["revision"], funded_at=event["funded_at"]))
        elif action == "operator_changed":
            new = event["settings"]
            expected = dict(settings, revision=settings["revision"]+1)
            c = event["command"]
            changed_ms = int(common.dt(event["changed_at"]).timestamp()*1000)
            if c:
                common.require(c["id"] not in commands and c["run_id"] in runs
                               and c["expected_revision"] == settings["revision"], "invalid control command")
                commands.add(c["id"])
                operation = c["action"]
                if operation["action"] in ("pause", "resume"):
                    expected.update(paused=operation["action"] == "pause", entry_epoch=settings["entry_epoch"]+1)
                elif operation["action"] == "set_budget":
                    budget(operation["amount"])
                    expected.update(pending_budget=operation["amount"], effective_window_ms=(changed_ms//300000+1)*300000)
                else:
                    raise ValueError("unknown operator command")
            else:
                common.require(settings["effective_window_ms"] is not None
                               and changed_ms >= settings["effective_window_ms"], "premature activation")
                expected.update(budget=settings["pending_budget"], pending_budget=None, effective_window_ms=None)
            common.require(new == expected, "control state does not match command/activation")
            budget(new["budget"])
            settings = new
        elif action == "observed":
            e = event["evaluation"]
            common.require(e["run_id"] in runs and e["evaluation_id"] not in observations, "invalid observation")
            observations[e["evaluation_id"]] = e
        elif action == "attempted":
            a = event["attempt"]; e = a["evaluation"]; key = a["attempt_id"]
            common.require(key not in positions and e == observations.get(e["evaluation_id"])
                           and a["arm"] == "eth_cap_097" and e["slug"] not in markets
                           and e["window_start_ms"] >= registered["forward_window_start_ms"], "invalid/duplicate attempt")
            common.require(not settings["paused"] and a["parameter_revision"] == settings["revision"]
                           and a["entry_epoch"] == settings["entry_epoch"]
                           and common.dec(a["budget_limit"]) == budget(settings["budget"]), "attempt control revision mismatch")
            ask, fee, reserve, quantity = map(common.dec, (e["ask_price"], e["fee_per_share"], a["reserved_cost"], a["requested"]))
            common.require(e["asset"] in ("btc", "eth") and D(".96") <= ask <= (D(".97") if e["asset"] == "eth" else D(".98"))
                           and quantity > 0 and reserve == quantity*(ask+fee) and 0 < reserve <= budget(settings["budget"])
                           and reserve <= cash, "invalid reservation")
            cash -= reserve; positions[key] = {"attempt": a}; markets.add(e["slug"])
        elif action == "executed":
            p = positions[event["attempt_id"]]; a = p["attempt"]; e = a["evaluation"]; x = event["execution"]
            common.require("execution" not in p and all(x[k] == e[k] for k in ("evaluation_id", "run_id", "slug")), "execution identity mismatch")
            fill, cost = common.dec(x["filled"]), common.dec(x["cost"])
            common.require(common.dec(x["requested"]) == common.dec(a["requested"])
                           and common.dec(x["limit_price"]) == common.dec(e["ask_price"])
                           and 0 <= fill <= common.dec(a["requested"]) and 0 <= cost <= common.dec(a["reserved_cost"]), "execution exceeds reservation")
            common.require(x["outcome"] in ("filled", "no_fill", "error")
                           and ((fill > 0 and cost > 0) if x["outcome"] == "filled" else (fill == 0 and cost == 0)), "invalid execution outcome")
            common.require(common.dt(x["simulated_at"]) >= common.dt(e["evaluated_at"]), "execution predates attempt")
            if fill:
                common.require(not settings["paused"] and settings["entry_epoch"] == a["entry_epoch"]
                               and common.dt(x["simulated_at"]) < common.dt(e["end_date"]), "fill committed after pause/expiry")
            cash += common.dec(a["reserved_cost"])-cost; p["execution"] = x
        elif action == "settled":
            p = positions[event["attempt_id"]]; x = p["execution"]; e = p["attempt"]["evaluation"]
            common.require("settlement" not in p and type(event["won"]) is bool
                           and common.dt(event["resolved_at"]) >= common.dt(e["end_date"]), "invalid settlement")
            payout = common.dec(x["filled"]) if event["won"] else D(0)
            common.require(common.dec(event["payout"]) == payout and common.dec(event["pnl"]) == payout-common.dec(x["cost"]), "settlement amount mismatch")
            up_won = e["buy_yes"] == event["won"]
            common.require(labels.setdefault(e["slug"], up_won) == up_won, "conflicting market labels")
            p["settlement"] = event; cash += payout
        else:
            raise ValueError("unknown event: " + action)
    rows = list(positions.values()); stats = common.summarize(rows)
    common.require(cash == D(100)+deposits+common.dec(stats["pnl"])-common.dec(stats["pending_cost"])-common.dec(stats["unknown_reserved_cash"]), "cash conservation failed")
    revisions = sorted({p["attempt"]["parameter_revision"] for p in rows})
    return dict(source_sha256=hashlib.sha256(raw).hexdigest(), settings=settings, cash=str(cash),
                capital=str(D(100)+deposits), deposits=str(deposits),
                equity=str(cash+common.dec(stats["pending_cost"])+common.dec(stats["unknown_reserved_cash"])),
                funding_events=funding_events, total=stats,
                by_asset={asset: common.summarize([p for p in rows if p["attempt"]["evaluation"]["asset"] == asset]) for asset in ("btc", "eth")},
                by_revision={str(v): dict(budget_limit=next(p["attempt"]["budget_limit"] for p in rows if p["attempt"]["parameter_revision"] == v),
                                         **common.summarize([p for p in rows if p["attempt"]["parameter_revision"] == v])) for v in revisions},
                interpretation="adjustable candidate PAPER only; deposits are capital, never trading PnL; separate from frozen research; changing size is not a proportional-profit forecast")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(analyze(args.input.read_bytes()), ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
