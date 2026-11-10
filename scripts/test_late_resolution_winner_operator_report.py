import copy
import json
import unittest
from decimal import Decimal as D
import late_resolution_winner_operator_report as report


def fixture():
    settings = dict(revision=0, entry_epoch=0, paused=True, budget="5", pending_budget=None, effective_window_ms=None)
    events = [dict(action="registered", version=2, study_id="winner-operator-paper-v2",
                   forward_window_start_ms=1793837100000, configuration=dict(
                       paper_only=True, automatic_promotion=False, initial_cash_per_arm="100", max_order_usdc="100",
                       initial_budget="5", initial_paused=True, arms={"eth_cap_097": dict(btc_max_ask="0.98", eth_max_ask="0.97")})),
              dict(action="run_started", run_id="r")]
    def change(action, stamp, **updates):
        settings.update(updates, revision=settings["revision"]+1)
        command = None if action is None else dict(id=str(settings["revision"]), run_id="r", expected_revision=settings["revision"]-1, action=action)
        events.append(dict(action="operator_changed", command=command, settings=dict(settings), changed_at=stamp))
    change(dict(action="resume"), "2026-11-05T00:00:01Z", paused=False, entry_epoch=1)
    change(dict(action="set_budget", amount="10"), "2026-11-05T00:00:02Z", pending_budget="10", effective_window_ms=1793837100000)
    change(None, "2026-11-05T00:05:00Z", budget="10", pending_budget=None, effective_window_ms=None)
    e = dict(evaluation_id="e", run_id="r", slug="eth-updown-5m-1793837100", asset="eth", window_start_ms=1793837100000,
             evaluated_at="2026-11-05T00:09:05Z", end_date="2026-11-05T00:10:00Z", buy_yes=True, ask_price="0.97", fee_per_share="0.001")
    events.append(dict(action="observed", evaluation=e))
    a = dict(attempt_id="a", arm="eth_cap_097", evaluation=e, parameter_revision=3, entry_epoch=1, budget_limit="10", requested="10.29", reserved_cost="9.99159")
    events.append(dict(action="attempted", attempt=a))
    x = dict(evaluation_id="e", run_id="r", slug=e["slug"], simulated_at="2026-11-05T00:09:05.3Z", requested="10.29", limit_price="0.97", outcome="filled", filled="10.29", cost="9.99159")
    events.append(dict(action="executed", attempt_id="a", execution=x))
    events.append(dict(action="settled", attempt_id="a", won=True, payout="10.29", pnl="0.29841", resolved_at="2026-11-05T00:16:00Z"))
    return events


def encode(events):
    return ("\n".join(json.dumps(e) for e in events)+"\n").encode()


class Audit(unittest.TestCase):
    def test_exact_accounting_and_revision_group(self):
        r = report.analyze(encode(fixture()))
        self.assertEqual(D(r["cash"]), D("100.29841"))
        self.assertEqual(r["total"]["wins"], 1)
        self.assertEqual(D(r["by_revision"]["3"]["pnl"]), D("0.29841"))
        self.assertEqual(r["by_revision"]["3"]["budget_limit"], "10")

    def test_rejects_old_revision_and_wrong_fill_amount(self):
        for field, value in [("parameter_revision", 2), ("budget_limit", "5")]:
            e = fixture(); e[6]["attempt"][field] = value
            with self.assertRaises(ValueError): report.analyze(encode(e))
        e = fixture(); e[7]["execution"]["cost"] = "10.1"
        with self.assertRaises(ValueError): report.analyze(encode(e))

    def test_pending_and_zero_exposure_pause_cancellation(self):
        e = fixture()[:-1]
        pending = report.analyze(encode(e))
        self.assertEqual(pending["total"]["pending_fills"], 1)
        self.assertEqual(D(pending["cash"]), D("90.00841"))
        pause = dict(action="operator_changed", command=dict(id="4", run_id="r", expected_revision=3, action=dict(action="pause")),
                     settings=dict(revision=4, entry_epoch=2, paused=True, budget="10", pending_budget=None, effective_window_ms=None), changed_at="2026-11-05T00:09:05.1Z")
        e.insert(7, pause)
        with self.assertRaises(ValueError): report.analyze(encode(e))
        x = e[8]["execution"]; x.update(outcome="error", filled="0", cost="0")
        r = report.analyze(encode(e))
        self.assertEqual(D(r["cash"]), D(100))
        self.assertEqual(r["total"]["losses"], 0)
        self.assertEqual(r["total"]["execution_errors"], 1)

    def test_rejects_bad_activation_state_and_incomplete_tail(self):
        for replacement in ["2026-11-05T00:04:59Z", "2026-11-04T00:05:00Z"]:
            e = fixture(); e[4]["changed_at"] = replacement
            with self.assertRaises(ValueError): report.analyze(encode(e))
        e = fixture(); e[4]["settings"]["budget"] = "50"
        with self.assertRaises(ValueError): report.analyze(encode(e))
        with self.assertRaises(ValueError): report.analyze(encode(fixture())[:-1])

    def funding(self, revision=4, amount="500", identity="credit-1"):
        return dict(action="operator_funded", amount=amount, revision=revision,
                    funded_at="2026-11-05T00:20:00Z", command=dict(id=identity, run_id="r",
                        expected_revision=revision-1, action=dict(action="deposit", amount=amount)))

    def test_funding_preserves_loss_and_separates_principal_from_pnl(self):
        e=fixture(); e[-1].update(won=False,payout="0",pnl="-9.99159")
        e.append(self.funding());r=report.analyze(encode(e))
        self.assertEqual(D(r["cash"]),D("590.00841"))
        self.assertEqual(D(r["capital"]),D(600));self.assertEqual(D(r["deposits"]),D(500))
        self.assertEqual(r["total"]["losses"],1);self.assertEqual(D(r["total"]["pnl"]),D("-9.99159"))
        self.assertFalse(r["settings"]["paused"]);self.assertEqual(r["settings"]["budget"],"10")
        self.assertEqual(r["by_revision"]["3"]["losses"],1)
        e.append(self.funding(5,"0.01","credit-2"));r=report.analyze(encode(e))
        self.assertEqual(D(r["cash"]),D("590.01841"));self.assertEqual(D(r["capital"]),D("600.01"))

    def test_funding_duplicate_stale_wrong_action_and_invalid_amount_fail_closed(self):
        e=fixture();e.append(self.funding())
        duplicate=self.funding(5,"500","credit-1")
        with self.assertRaises(ValueError): report.analyze(encode(e+[duplicate]))
        for amount in ["0","-1","0.001","1000000.01"]:
            with self.assertRaises(ValueError): report.analyze(encode(fixture()+[self.funding(amount=amount)]))
        for field,value in [("expected_revision",1),("run_id","unknown")]:
            f=self.funding();f["command"][field]=value
            with self.assertRaises(ValueError): report.analyze(encode(fixture()+[f]))
        f=self.funding();f["command"]["action"]["amount"]="499"
        with self.assertRaises(ValueError): report.analyze(encode(fixture()+[f]))
        f=self.funding();f["command"]["action"]["action"]="set_budget"
        with self.assertRaises(ValueError): report.analyze(encode(fixture()+[f]))

    def test_funding_keeps_pending_reservation_and_cash_conservation(self):
        e=fixture()[:-2] # attempt reserved, execution not known yet
        e.append(self.funding());r=report.analyze(encode(e))
        self.assertEqual(r["total"]["execution_unknown"],1)
        self.assertEqual(D(r["cash"]),D("590.00841"))
        self.assertEqual(D(r["equity"]),D(600));self.assertEqual(D(r["total"]["pnl"]),D(0))


if __name__ == "__main__":
    unittest.main()
