import copy
import json
import math
import tempfile
import unittest
from pathlib import Path

import digital_fair_value_incremental_diagnostic as d
import test_digital_fair_value_probability_calibration as fixtures


def plan():
    value = json.loads((d.c.ROOT/"scripts/digital_fair_value_incremental_013_plan.json").read_text())
    value["bootstrap_draws"] = 200
    return value


class IncrementalDiagnosticTests(unittest.TestCase):
    def test_prethreshold_choice_does_not_read_outcome_or_delayed_quotes(self):
        r = fixtures.row()
        first = d.best_opportunity(r, .45, fixtures.policy())
        altered = copy.deepcopy(r)
        altered["won"] = 0
        altered["episode"]["delayed"] = {}
        self.assertEqual(first, d.best_opportunity(altered, .45, fixtures.policy()))
        self.assertEqual(first["side"], "down")
        self.assertAlmostEqual(first["edge"], d.c.signal_choice(r, .45, fixtures.policy())["edge"])

    def test_all_windows_have_one_direction_even_without_edge(self):
        r = fixtures.row()
        # Move down ask up to remove the cheap raw opportunity.
        r["episode"]["signal"]["books"]["down"]["asks"][0]["price"] = .50
        best = d.best_opportunity(r, .55, fixtures.policy())
        self.assertLess(best["edge"], fixtures.policy()["min_edge"])
        self.assertIn(best["side"], ["up", "down"])
        self.assertIsNone(d.c.signal_choice(r, .55, fixtures.policy()))

    def test_fixed_bins_include_negative_values_and_exact_boundaries(self):
        self.assertEqual(d.disagreement_bin(-.4, [.1, .25]), "lt_010")
        self.assertEqual(d.disagreement_bin(.1, [.1, .25]), "010_025")
        self.assertEqual(d.disagreement_bin(.25, [.1, .25]), "ge_025")

    def test_nested_models_agree_when_original_disagreement_is_zero(self):
        samples = [dict(p=.3+(i % 3)*.2, market=.3+(i % 3)*.2, y=i % 2) for i in range(150)]
        control = d.fit_market_offset(samples, False, plan())
        added = d.fit_market_offset(samples, True, plan())
        self.assertAlmostEqual(added["increment_coefficient"], 0)
        for r in samples:
            self.assertAlmostEqual(d.offset_forecast(r, control), d.offset_forecast(r, added))

    def test_signed_coefficient_detects_positive_and_negative_model_information(self):
        positive = [dict(p=.2 if i % 2 == 0 else .8, market=.5, y=i % 2) for i in range(100)]
        negative = [dict(r, y=1-r["y"]) for r in positive]
        a = d.fit_market_offset(positive, True, plan())
        b = d.fit_market_offset(negative, True, plan())
        self.assertGreater(a["increment_coefficient"], 0)
        self.assertLess(b["increment_coefficient"], 0)
        self.assertGreater(d.offset_forecast(positive[1], a), .5)
        self.assertLess(d.offset_forecast(positive[1], b), .5)

    def test_incremental_training_does_not_read_pnl_or_selected_direction(self):
        samples = [dict(p=.2+(i % 3)*.3, market=.4, y=i % 2) for i in range(100)]
        changed = [dict(r, side="down", pnl=1e20, delayed_price=-100) for r in samples]
        self.assertEqual(d.fit_market_offset(samples, True, plan()), d.fit_market_offset(changed, True, plan()))

    def test_future_outcome_mutation_cannot_change_prior_fold_models(self):
        dataset = [fixtures.record(day*86400+i*300, .2+(i % 3)*.3, i % 2)
                   for day in range(5) for i in range(40)]
        evaluated = [r for r in dataset if r["day"] in ["1970-01-04", "1970-01-05"]]
        def opportunities(rs):
            return [dict(slug=r["slug"], day=r["day"], side="up", q=r["p"], m=r["market"],
                         y=r["y"], has_edge=True, funded=True) for r in rs]
        folds = {day: dict(training_samples=len(d.c.training_for_day(dataset, day)))
                 for day in ["1970-01-04", "1970-01-05"]}
        a, _ = d.incremental_evaluation(dataset, opportunities(evaluated), folds, [], plan())
        altered = copy.deepcopy(dataset)
        for r in altered:
            if r["day"] == "1970-01-05":
                r["y"] = 1-r["y"]
        b, _ = d.incremental_evaluation(altered, opportunities(evaluated), folds, [], plan())
        self.assertEqual(a["folds"], b["folds"])
        for f in a["folds"]:
            self.assertLess(f["maximum_training_window_end_ms"], f["fit_at_ms"])
            self.assertLessEqual(f["maximum_training_known_at_ms"], f["fit_at_ms"])

    def test_negative_increment_never_authorizes_reversing_the_signal(self):
        increment = dict(n=215, complete_days=list(range(7)), complete_fold_positive_increment_fraction=0,
                         all_observed_mean_differences=dict(logloss_increment_minus_market_adjusted=-.02),
                         complete_day_intervals=dict(metrics=dict(
                             brier_increment_minus_market_adjusted=dict(available=True, upper=-.01))))
        result = d.decision(increment, dict(totals=dict(net_pnl=100)), dict(pnl=100), plan())
        self.assertFalse(result["positive_increment_supported"])
        self.assertFalse(result["trading_accepted"])
        self.assertFalse(result["launch_new_observation"])
        self.assertIn("model_disagreement_not_stably_positive", result["reasons"])

    def test_cashflow_identity_handles_depth_fees_slip_and_price_improvement(self):
        r = fixtures.row()
        r["episode"]["delayed"]["books"]["down"]["asks"] = [dict(price=.20, size=1), dict(price=.30, size=1000)]
        p = fixtures.policy()
        t = d.c.terminal.replay_with_fee(r, .45, p)
        self.assertEqual(t["status"], "filled")
        decomposition = d.accounting(r, t, .45, p, 1e-8)
        self.assertLess(decomposition["quote_move_cost"], 0)
        self.assertGreater(decomposition["depth_cost"], 0)
        self.assertGreater(decomposition["slippage_cost"], 0)
        self.assertGreater(decomposition["fees"], 0)
        self.assertAlmostEqual(decomposition["net_pnl"], -5)
        self.assertAlmostEqual(decomposition["cashflow_identity_error"], 0)
        self.assertAlmostEqual(decomposition["expected_identity_error"], 0)

    def test_cashflow_identity_rejects_changed_archived_pnl(self):
        r, p = fixtures.row(), fixtures.policy()
        t = d.c.terminal.replay_with_fee(r, .9, p)
        t["settlement_pnl"] += 1
        with self.assertRaisesRegex(ValueError, "cashflow_identity_failed"):
            d.accounting(r, t, .9, p, 1e-8)

    def test_archived_fill_still_requires_fresh_linked_delayed_book(self):
        r, p = fixtures.row(), fixtures.policy()
        t = d.c.terminal.replay_with_fee(r, .9, p)
        r["episode"]["delayed"]["books"]["up"]["http"]["exchange_timestamp"] -= 2000
        with self.assertRaisesRegex(ValueError, "book_freshness_invalid"):
            d.accounting(r, t, .9, p, 1e-8)

    def test_zero_row_days_remain_in_day_bootstrap(self):
        rs = [dict(day="day0", value=1.0)]
        ci = d.day_intervals(rs, [f"day{i}" for i in range(5)], ["value"], plan())
        self.assertEqual(ci["blocks"], 5)
        self.assertEqual(ci["metrics"]["value"]["lower"], 1)
        self.assertEqual(ci["metrics"]["value"]["upper"], 1)

    def test_same_day_contrast_uses_one_common_day_resample(self):
        a = [dict(day=str(i), optimism=i/10) for i in range(5)]
        b = [dict(day=str(i), optimism=i/10-.2) for i in range(5)]
        ci = d.stage_contrast(a, b, list(map(str, range(5))), plan())
        self.assertAlmostEqual(ci["optimism_mean_difference"], .2)
        self.assertAlmostEqual(ci["interval"]["lower"], .2)
        self.assertAlmostEqual(ci["interval"]["upper"], .2)


if __name__ == "__main__":
    unittest.main()
