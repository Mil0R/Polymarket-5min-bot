import copy
import json
import tempfile
import unittest
from datetime import datetime
from decimal import Decimal
from pathlib import Path

import late_resolution_winner_combination_study as study


START = int(study.date_time("2026-10-05T10:00:00Z").timestamp())
REGISTRATION = study.date_time("2026-10-05T10:07:00Z")


def encoded(events):
    return b"".join((json.dumps(event)+"\n").encode() for event in events)


def run_event(run_id=study.FORWARD_RUN, started_at="2026-09-29T02:37:20Z"):
    return {"action": "run_started", "run_id": run_id, "started_at": started_at,
            "mode": "paper", "hybrid_paper_enabled": True}


def events_for_trade(key="a", window=START, won=True, run_id=study.FORWARD_RUN,
                     asset="eth", offset=260, filled="5.14", cost="4.99627018"):
    slug = f"{asset}-updown-5m-{window}"
    evaluated = study.iso(datetime.fromtimestamp(window+offset, study.UTC))
    executed = study.iso(datetime.fromtimestamp(window+offset+1, study.UTC))
    evaluation = {"evaluation_id": key, "run_id": run_id, "slug": slug, "asset": asset,
                  "candidate_stage": "rejected_shadow", "window_start_ms": window*1000,
                  "evaluated_at": evaluated, "seconds_remaining": 300-offset, "buy_yes": True,
                  "ask_price": "0.97", "model_probability": "0.60", "reversal_margin_ratio": .15,
                  "reversal_boundary_bps": 18, "model_observed_at_ms": (window+offset-1)*1000,
                  "failed_checks": ["ReversalBoundary", "NetEvTooLow"]}
    execution = {"evaluation_id": key, "run_id": run_id, "slug": slug, "simulated_at": executed,
                 "candidate_stage": "rejected_confirmed", "filled": filled, "cost": cost,
                 "outcome": "filled" if Decimal(filled) else "no_fill", "requested": "5.14"}
    events = [{"action": "evaluated", "evaluation": evaluation}, {"action": "shadow_executed", "execution": execution}]
    if won is not None:
        events.append({"action": "resolved", "evaluation_id": key, "slug": slug, "won": won,
                       "resolved_at": study.iso(datetime.fromtimestamp(window+350, study.UTC))})
    return events


def trades(events):
    return study.latest_main(study.parse_journals([("test.ndjson", encoded(events))])["trades"])


class FinancialJoinTests(unittest.TestCase):
    def test_executed_yes_loses_even_when_last_no_candidate_wins(self):
        executed = events_for_trade(won=False)
        last = copy.deepcopy(executed[0])
        last["evaluation"].update(evaluation_id="last-no", buy_yes=False,
                                  evaluated_at=study.iso(datetime.fromtimestamp(START+280, study.UTC)))
        rows = executed + [last, {"action": "resolved", "evaluation_id": "last-no",
                                 "slug": last["evaluation"]["slug"], "won": True},
                           {"action": "market_resolved", "selected_evaluation_id": "last-no", "won": True}]
        result = study.stats(trades(rows))
        self.assertEqual((result["wins"], result["losses"]), (0, 1))
        self.assertEqual(result["pnl"], "-4.99627018")

    def test_duplicates_and_sensitivity_do_not_inflate_main_pnl(self):
        rows = events_for_trade()
        sensitivity = copy.deepcopy(rows[1])
        sensitivity["execution"]["candidate_stage"] = "sensitivity_confirmed"
        result = study.stats(trades(rows+[rows[1], sensitivity]))
        self.assertEqual(result["attempts"], 1)
        self.assertEqual(result["pnl"], "0.14372982")

    def test_latest_main_attempt_per_market_can_be_no_fill(self):
        rows = events_for_trade("first") + events_for_trade("later", offset=280, filled="0", cost="0")
        result = study.stats(trades(rows))
        self.assertEqual((result["attempts"], result["settled_fills"], result["no_fills"]), (1, 0, 1))
        self.assertEqual(result["pnl"], "0")

    def test_pending_exposure_is_separate_from_settled_cost_and_pnl(self):
        rows = events_for_trade("pending", won=None) + events_for_trade("settled", window=START+300)
        result = study.stats(trades(rows))
        self.assertEqual(result["pending_fills"], 1)
        self.assertEqual(result["pending_cost"], "4.99627018")
        self.assertEqual(result["cost"], "4.99627018")
        self.assertEqual(result["pnl"], "0.14372982")

    def test_conflicting_labels_and_string_boolean_are_rejected(self):
        rows = events_for_trade()
        conflicting = dict(rows[-1], won=False)
        with self.assertRaisesRegex(ValueError, "conflicting resolution"):
            trades(rows+[conflicting])
        rows[-1]["won"] = "false"
        with self.assertRaisesRegex(ValueError, "must be a boolean"):
            trades(rows)

    def test_conflicting_main_records_are_not_silently_overwritten(self):
        rows = events_for_trade()
        duplicate = copy.deepcopy(rows[1])
        duplicate["execution"]["cost"] = "5.01"
        with self.assertRaisesRegex(ValueError, "conflicting main execution"):
            trades(rows+[duplicate])

    def test_nonzero_no_fill_and_missing_evaluation_fail(self):
        rows = events_for_trade()
        rows[1]["execution"]["outcome"] = "no_fill"
        with self.assertRaisesRegex(ValueError, "no_fill has exposure"):
            trades(rows)
        with self.assertRaisesRegex(ValueError, "missing its evaluation"):
            trades(events_for_trade()[1:])

    def test_incomplete_tail_is_rejected(self):
        raw = encoded(events_for_trade()).rstrip(b"\n")
        with self.assertRaisesRegex(ValueError, "incomplete last line"):
            study.parse_journals([("test.ndjson", raw)])


class FeatureAndRuleTests(unittest.TestCase):
    def test_missing_or_stale_probability_is_unknown_not_zero(self):
        rows = events_for_trade()
        rows[0]["evaluation"].update(model_probability="0", model_observed_at_ms=None)
        trade = trades(rows)[0]
        self.assertIsNone(study.features(trade)["probability"])
        result = study.evaluate_rules([trade], study.candidate_rules())["probability_070"]
        self.assertEqual(result["unknown"]["eth"]["attempts"], 1)
        self.assertEqual(result["rejected"]["eth"]["attempts"], 0)
        rows[0]["evaluation"]["model_observed_at_ms"] = (START+259)*1000
        rows[0]["evaluation"]["failed_checks"].append("StalePrice")
        self.assertIsNone(study.features(trades(rows)[0])["probability"])

    def test_negative_ratio_keeps_direction_and_exact_threshold_is_inclusive(self):
        rows = events_for_trade()
        rows[0]["evaluation"].update(model_probability="0.70", reversal_margin_ratio=-.2)
        values = study.features(trades(rows)[0])
        self.assertEqual(values["ratio"], Decimal("-.2"))
        self.assertTrue(study.condition_result(study.compare("probability", "ge", ".70"), values))
        self.assertFalse(study.condition_result(study.compare("ratio", "ge", ".20"), values))

    def test_three_way_unknown_is_only_used_when_needed(self):
        rows = events_for_trade()
        rows[0]["evaluation"].update(ask_price=".96", model_observed_at_ms=None)
        rule = next(r for r in study.candidate_rules() if r["id"] == "exclude_joint_risk")
        self.assertTrue(study.condition_result(rule["condition"], study.features(trades(rows)[0])))
        rows[0]["evaluation"]["ask_price"] = ".97"
        self.assertIsNone(study.condition_result(rule["condition"], study.features(trades(rows)[0])))

    def test_partition_conserves_financial_totals_for_every_rule(self):
        rows = events_for_trade("loss", won=False) + events_for_trade("win", window=START+300)
        rows += events_for_trade("unknown", window=START+600)
        rows[-3]["evaluation"]["model_observed_at_ms"] = None
        selected = trades(rows)
        control = study.stats(selected)
        for rule in study.evaluate_rules(selected, study.candidate_rules()).values():
            groups = [rule[name]["all"] for name in ("kept", "rejected", "unknown")]
            self.assertEqual(sum(g["attempts"] for g in groups), control["attempts"])
            self.assertEqual(sum(Decimal(g["pnl"]) for g in groups), Decimal(control["pnl"]))
            self.assertEqual(sum(g["losses"] for g in groups), 1)


class RegistrationAndForwardTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.journal = self.root / "journal.ndjson"
        self.plan_path = self.root / "plan.json"

    def register(self, rows=None, now=REGISTRATION):
        self.journal.write_bytes(encoded([run_event()]+(rows if rows is not None else events_for_trade())))
        return study.register_plan(self.root, self.plan_path, ["journal.ndjson"], now)

    def append(self, events):
        with self.journal.open("ab") as stream:
            stream.write(encoded(events))

    def analyze(self):
        return study.analyze_plan(self.root, self.plan_path)

    def test_registration_refuses_overwrite_and_empty_forward_is_collecting(self):
        plan = self.register()
        self.assertEqual(plan["forward_window_start"], "2026-10-05T10:10:00Z")
        with self.assertRaisesRegex(ValueError, "refusing overwrite"):
            study.register_plan(self.root, self.plan_path, ["journal.ndjson"], REGISTRATION)
        result = self.analyze()
        self.assertEqual(result["forward"]["control"]["all"]["settled_fills"], 0)
        self.assertFalse(result["progress"]["overall_sample_milestones_met"])
        for progress in result["progress"]["rule_progress"].values():
            self.assertIsNone(progress["nominal_wilson_above_weighted_break_even"])

    def test_forward_uses_market_start_not_later_sync_or_execution_time(self):
        self.register()
        self.append(events_for_trade("gap", window=START+300))  # 10:09:21 execution, window before 10:10
        self.append(events_for_trade("forward", window=START+600, won=False))
        report = self.analyze()
        self.assertEqual(report["excluded_new_attempts"]["pre_forward_gap"]["all"]["attempts"], 1)
        self.assertEqual(report["forward"]["control"]["eth"]["losses"], 1)
        self.assertEqual(report["forward"]["control"]["eth"]["pnl"], "-4.99627018")

    def test_registration_on_boundary_still_starts_next_complete_window(self):
        plan = self.register(now=study.date_time("2026-10-05T10:10:00Z"))
        self.assertEqual(plan["forward_window_start"], "2026-10-05T10:15:00Z")

    def test_baseline_pending_label_is_followup_not_new_forward_fill(self):
        plan = self.register(events_for_trade(won=None))
        self.append([events_for_trade()[-1]])
        report = self.analyze()
        self.assertEqual(plan["baseline_at_registration"]["eth"]["settled_fills"], 0)
        self.assertEqual(report["baseline_frozen"]["control"]["eth"]["pending_fills"], 1)
        self.assertEqual(report["baseline_with_later_labels"]["eth"]["wins"], 1)
        self.assertEqual(report["forward"]["control"]["eth"]["settled_fills"], 0)

    def test_old_market_retry_is_quarantined(self):
        self.register()
        self.append(events_for_trade("retry", offset=280, won=False))
        report = self.analyze()
        self.assertEqual(report["baseline_frozen"]["control"]["eth"]["wins"], 1)
        self.assertEqual(report["excluded_new_attempts"]["baseline_market_overlap"]["eth"]["losses"], 1)
        self.assertIn("new_attempt_in_baseline_market", report["protocol_deviations"])

    def test_new_run_is_flagged_even_before_it_trades(self):
        self.register()
        self.append([run_event("new-run", "2026-10-05T10:08:00Z")])
        report = self.analyze()
        self.assertIn("new_run_since_registration", report["protocol_deviations"])
        self.append(events_for_trade("new-run-trade", window=START+600, run_id="new-run"))
        report = self.analyze()
        self.assertEqual(report["forward"]["control"]["eth"]["attempts"], 0)
        self.assertEqual(report["excluded_new_attempts"]["unregistered_run"]["eth"]["attempts"], 1)

    def test_prefix_corruption_and_truncation_fail(self):
        self.register()
        original = self.journal.read_bytes()
        self.journal.write_bytes(original.replace(b'"0.60"', b'"0.61"'))
        with self.assertRaisesRegex(ValueError, "prefix changed"):
            self.analyze()
        self.journal.write_bytes(original[:-1])
        with self.assertRaisesRegex(ValueError, "truncated"):
            self.analyze()

    def test_editing_frozen_rules_invalidates_registration_checksum(self):
        plan = self.register()
        plan["rules"][1]["condition"]["value"] = ".65"
        self.plan_path.write_text(json.dumps(plan))
        with self.assertRaisesRegex(ValueError, "plan checksum changed"):
            self.analyze()

    def test_all_no_fill_group_never_meets_sample_milestone(self):
        self.register()
        for day in range(8):
            self.append(events_for_trade(f"none-{day}", window=START+600+day*86400, filled="0", cost="0"))
        report = self.analyze()
        self.assertEqual(report["forward"]["control"]["eth"]["no_fills"], 8)
        self.assertEqual(report["forward"]["control"]["eth"]["settled_fill_days"], 0)
        self.assertFalse(report["progress"]["overall_sample_milestones_met"])


if __name__ == "__main__":
    unittest.main()
