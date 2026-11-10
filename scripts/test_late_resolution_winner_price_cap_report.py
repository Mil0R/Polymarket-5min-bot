import copy
import json
import unittest
from decimal import Decimal as D

from late_resolution_winner_price_cap_report import ARMS, analyze


def fixture():
    return [{"action": "registered", "version": 1, "study_id": "eth-ask-cap-097-v1",
             "registered_at": "2026-10-26T00:00:01Z", "forward_window_start_ms": 1792973100000,
             "configuration": {"paper_only": True, "automatic_promotion": False,
                               "initial_cash_per_arm": "100", "max_order_usdc": "5",
                               "arms": {ARMS[0]: {"btc_max_ask": "0.98", "eth_max_ask": "0.98"},
                                        ARMS[1]: {"btc_max_ask": "0.98", "eth_max_ask": "0.97"}}}},
            {"action": "run_started", "run_id": "007", "started_at": "2026-10-26T00:00:01Z"}]


def attempt(events, arm, ask="0.97", tick="01", won=True, outcome="filled", settle=True):
    identity = f"{arm}-{tick}"
    e = {"evaluation_id": identity, "run_id": "007", "asset": "eth",
         "slug": "eth-updown-5m-1792973100", "window_start_ms": 1792973100000,
         "ask_price": ask, "fee_per_share": "0.001", "buy_yes": True,
         "evaluated_at": f"2026-10-26T00:09:{tick}Z", "end_date": "2026-10-26T00:10:00Z"}
    reserve = str(D(5) * (D(ask) + D(".001")))
    events += [{"action": "observed", "evaluation": e,
                "decisions": [{"arm": a, "reason": "attempt", "confirmations": 2} for a in ARMS]},
               {"action": "attempted", "attempt": {"attempt_id": identity, "arm": arm,
                                                      "evaluation": copy.deepcopy(e), "requested": "5", "reserved_cost": reserve}}]
    if outcome is None:
        return
    fill = "5" if outcome == "filled" else "0"
    cost = reserve if outcome == "filled" else "0"
    events += [{"action": "executed", "attempt_id": identity, "execution": {
        "evaluation_id": identity, "run_id": "007", "slug": e["slug"], "limit_price": ask,
        "requested": "5", "filled": fill, "cost": cost, "outcome": outcome,
        "simulated_at": f"2026-10-26T00:09:{tick}.270Z"}}]
    if settle:
        payout = D(fill) if won else D(0)
        events += [{"action": "settled", "attempt_id": identity, "won": won,
                    "payout": str(payout), "pnl": str(payout-D(cost)), "resolved_at": "2026-10-26T00:11:00Z"}]


def report(events):
    return analyze(("\n".join(json.dumps(e) for e in events)+"\n").encode())


class PriceCapReportTests(unittest.TestCase):
    def test_later_entry_uses_its_own_price_and_preserves_cash(self):
        events=fixture()
        attempt(events,ARMS[0],"0.98","01")
        attempt(events,ARMS[1],"0.97","20")
        r=report(events)
        self.assertEqual(D(r["arms"][ARMS[0]]["cash"]),D("100.095"))
        self.assertEqual(D(r["arms"][ARMS[1]]["cash"]),D("100.145"))
        self.assertEqual(D(r["matched_markets"]["eth"]["candidate_minus_control"]),D(".050"))
        self.assertEqual(r["matched_markets"]["eth"]["later_entry_markets"],1)

    def test_cap_rejection_is_zero_exposure_on_completed_market(self):
        events=fixture();attempt(events,ARMS[0],"0.98","01",won=False)
        r=report(events)
        self.assertEqual(r["arms"][ARMS[1]]["total"]["attempts"],0)
        self.assertEqual(D(r["matched_markets"]["eth"]["candidate_minus_control"]),D("4.905"))

    def test_pending_unknown_and_execution_errors_are_not_matched_profit(self):
        for outcome in (None,"error"):
            events=fixture();attempt(events,ARMS[0],outcome=outcome)
            r=report(events)
            self.assertEqual(r["arms"][ARMS[0]]["total"]["settled_fills"],0)
            self.assertEqual(r["matched_markets"]["eth"]["complete_markets"],0)
            self.assertEqual(D(r["arms"][ARMS[0]]["total"]["pnl"]),0)
        events=fixture();attempt(events,ARMS[0],settle=False)
        r=report(events)
        self.assertEqual(r["arms"][ARMS[0]]["total"]["pending_fills"],1)
        self.assertEqual(r["matched_markets"]["eth"]["complete_markets"],0)

    def test_no_fill_does_not_inherit_counterfactual_profit(self):
        events=fixture();attempt(events,ARMS[0],outcome="no_fill")
        r=report(events)
        self.assertEqual(r["arms"][ARMS[0]]["total"]["wins"],0)
        self.assertEqual(D(r["arms"][ARMS[0]]["cash"]),100)
        self.assertEqual(D(r["matched_markets"]["eth"]["candidate_minus_control"]),0)

    def test_ambiguous_and_invalid_records_fail_closed(self):
        events=fixture();attempt(events,ARMS[1],"0.98")
        with self.assertRaisesRegex(ValueError,"exceeded .97"):report(events)
        events=fixture();attempt(events,ARMS[0]);events.append(copy.deepcopy(events[-1]))
        with self.assertRaisesRegex(ValueError,"duplicate settlement"):report(events)
        with self.assertRaisesRegex(ValueError,"incomplete journal"):analyze(b'{"action":')

    def test_cross_arm_market_labels_must_agree(self):
        events=fixture();attempt(events,ARMS[0],won=True)
        attempt(events,ARMS[1],tick="02",won=False)
        with self.assertRaisesRegex(ValueError,"conflicting market"):report(events)


if __name__ == "__main__":
    unittest.main()
