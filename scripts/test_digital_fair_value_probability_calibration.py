import copy
import math
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import digital_fair_value_probability_calibration as c


def policy():
    return dict(c.terminal.POLICY, **c.terminal.risk.DEFAULTS, fee_source="observed_required",
                require_opening_audit=True, failure_rate=0.0, stress_failure_rate=0.04,
                stress_haircut=0.5, stress_slippage_bps=20.0)


def row(start=0, won=1):
    now = start*1000+270000
    def book(bid, ask):
        return dict(valid=True, best_bid=bid, best_ask=ask,
                    asks=[dict(price=ask, size=1000)],
                    http=dict(exchange_timestamp=now, rest_latency_ms=10))
    slug = f"eth-updown-5m-{start}"
    signal = dict(sampled_at_ms=now, opening_twap=100.0,
                  model_audit=dict(observation_age_ms=0, local_receive_age_ms=0,
                                   network_receive_latency_ms=0),
                  market_fee_audit=dict(verified=True, observed_at_ms=now, rate=.07,
                                        exponent=1, slug=slug),
                  opening_reference_audit=dict(observed_at_ms=now, official_price_to_beat=100.0),
                  baseline=dict(source="up_midpoint"), books=dict(up=book(.59, .60), down=book(.24, .25)))
    delayed = dict(sampled_at_ms=now+1000, actual_delay_ms=1000,
                   monotonic_delay_ms=1000, before_expiry=True, books=copy.deepcopy(signal["books"]))
    for b in delayed["books"].values():
        b["http"]["exchange_timestamp"] = now+1000
    return dict(slug=slug, asset="eth", time=start, won=won, market=.595,
                episode=dict(episode_id=slug, signal=signal, delayed=delayed, window_end_ms=start*1000+300000))


def record(start, p=.45, y=1):
    r = row(start, y)
    return dict(slug=r["slug"], time=start, day=c.utc_day(start), signal_ms=start*1000+270000,
                end_ms=start*1000+300000, known_at_ms=start*1000+600000,
                p=p, y=y, market=.595, shared=True, exclusion=None, row=r)


class CalibrationTests(unittest.TestCase):
    def test_logistic_learns_offset_and_keeps_monotonic_complement(self):
        samples = [dict(p=.7, market=.5, y=int(i % 4 == 0)) for i in range(200)]
        fitted = c.fit_logistic(samples)
        q = c.forecast(samples[0], fitted)
        self.assertLess(abs(q-.25), abs(.7-.25))
        self.assertGreater(fitted["slope"], 0)
        qs = [c.forecast(dict(p=p), fitted) for p in [.001, .1, .5, .9, .999]]
        self.assertEqual(qs, sorted(qs))
        self.assertTrue(all(0 < p < 1 for p in qs))
        self.assertTrue(all(p+(1-p) == 1 for p in qs))

    def test_shrink_closed_form_boundaries_and_missing_market(self):
        samples = [dict(p=.8, market=.2, y=0) for _ in range(10)]
        fitted = c.fit_shrink(samples)
        self.assertEqual(fitted["alpha"], 0)
        self.assertEqual(c.forecast(samples[0], fitted), .2)
        self.assertIsNone(c.forecast(dict(p=.8, market=None), fitted))
        self.assertEqual(c.fit_shrink([dict(p=.5, market=.5, y=0)])["alpha"], 1)
        with self.assertRaises(ValueError):
            c.fit_shrink([dict(p=.8, market=None, y=0)])

    def test_fit_ignores_profit_delayed_quotes_and_selected_direction(self):
        original = [dict(p=.2+i/200, market=.4, y=i % 2) for i in range(100)]
        changed = [dict(r, pnl=1e9, side="down", delayed_quote=-1, target_bps=-1e9) for r in original]
        self.assertEqual(c.fit_logistic(original), c.fit_logistic(changed))
        self.assertEqual(c.fit_shrink(original), c.fit_shrink(changed))

    def test_signal_gate_does_not_condition_on_future_execution_or_outcome(self):
        r = row()
        self.assertIsNone(c.signal_eligibility(r, policy()))
        self.assertIsNone(c.market_eligibility(r, policy()))
        changed = copy.deepcopy(r)
        changed["won"] = 0
        changed["episode"]["delayed"] = {}
        self.assertIsNone(c.signal_eligibility(changed, policy()))
        self.assertIsNone(c.market_eligibility(changed, policy()))
        self.assertEqual(c.terminal.causal_signal_policy(r, policy()),
                         c.terminal.causal_signal_policy(changed, policy()))
        changed["episode"]["signal"]["market_fee_audit"]["observed_at_ms"] += 1
        self.assertEqual(c.signal_eligibility(changed, policy()), "missing_causal_market_fee_schedule")

    def test_market_gate_rejects_stale_midpoint_even_if_other_side_fresh(self):
        r = row()
        r["episode"]["signal"]["books"]["up"]["http"]["exchange_timestamp"] -= 2000
        self.assertIsNone(c.signal_eligibility(r, policy()))
        self.assertEqual(c.market_eligibility(r, policy()), "stale_signal_midpoint")
        r = row()
        r["market"] = None
        self.assertEqual(c.market_eligibility(r, policy()), "missing_market_midpoint")

    def test_label_availability_and_terminal_embargo(self):
        r = record(0)
        r["known_at_ms"] = c.day_start("1970-01-03")
        self.assertEqual(c.training_for_day([r], "1970-01-02"), [])
        self.assertEqual(c.training_for_day([r], "1970-01-03"), [r])
        r["known_at_ms"] = 1
        r["end_ms"] = c.day_start("1970-01-02")-1000
        self.assertEqual(c.training_for_day([r], "1970-01-02"), [])

    def test_future_label_mutation_never_changes_earlier_fold_models(self):
        records = []
        for day in range(5):
            for i in range(40):
                records.append(record(day*86400+i*300, .1+(i % 5)*.18, i % 2))
        first, forecasts = c.rolling_forecasts(records)
        altered = copy.deepcopy(records)
        for r in altered:
            if r["day"] == "1970-01-05":
                r["y"] = 1-r["y"]
        second, changed = c.rolling_forecasts(altered)
        self.assertEqual(first, second)
        self.assertEqual(forecasts, changed)
        self.assertTrue(any(f["status"] == "evaluated" for f in first))
        for fold in first:
            if fold["status"] == "evaluated":
                self.assertLess(fold["maximum_training_window_end_ms"], fold["fit_at_ms"])
                self.assertLessEqual(fold["maximum_training_known_at_ms"], fold["fit_at_ms"])

    def test_recalibration_reselects_side_and_recomputes_cashflows(self):
        r = record(0, .45, 1)
        pol = policy()
        raw, rawtrades, _ = c.execute([r], {r["slug"]: .45}, pol)
        calibrated, trades, _ = c.execute([r], {r["slug"]: .9}, pol)
        self.assertEqual(rawtrades[0]["side"], "down")
        self.assertEqual(trades[0]["side"], "up")
        self.assertLess(raw["pnl"], 0)
        self.assertGreater(calibrated["pnl"], 0)
        self.assertAlmostEqual(calibrated["account"]["cash"], 1000+calibrated["pnl"])

    def test_paired_scores_keep_original_side_and_sample_not_reselected_trades(self):
        r = record(0, .45, 1)
        forecasts = {r["slug"]: dict(identity=.45, logistic=.9)}
        result, pairs = c.paired_review([r], forecasts, {r["slug"]: "down"}, "logistic", [])
        self.assertEqual(result["n"], 1)
        self.assertAlmostEqual(pairs[0]["p"], .1)
        self.assertEqual(pairs[0]["y"], 0)
        self.assertAlmostEqual(result["calibrated"]["brier"], .01)

    def test_zero_fill_covered_days_retained_in_cashflow_intervals(self):
        records = []
        for day in range(5):
            for slot in range(288):
                records.append(record(day*86400+slot*300))
        summary, ts, _ = c.execute(records, {}, policy())
        self.assertEqual(ts, [])
        self.assertEqual(summary["full_day_pnl_interval"]["blocks"], 5)
        self.assertEqual(summary["full_day_pnl_interval"]["lower"], 0)
        self.assertEqual(len(summary["daily_pnl"]), 5)

    def test_draft_protocol_never_auto_starts_or_reuses_diagnostic_gate(self):
        report = dict(selection=dict(method=None), future_sample_size_estimate=dict(available=False))
        proto = c.future_protocol(report, {}, "abc")
        self.assertEqual(proto["status"], "draft_not_started")
        self.assertIsNone(proto["start_utc"])
        self.assertFalse(proto["trading_acceptance"])
        self.assertTrue(proto["original_100_fills_5_days_diagnostic_gate_not_reused"])

    def test_market_clone_with_no_trades_blocks_future_strategy_launch(self):
        report = dict(selection=dict(method="market_shrink"),
                      strategies=dict(market_shrink=dict(fills=0)),
                      future_sample_size_estimate=dict(available=True))
        proto = c.future_protocol(report, dict(market_shrink=dict(alpha=0.0)), "abc")
        self.assertFalse(proto["launch_ready"])
        self.assertIn("frozen_calibrator_is_market_probability_not_model_edge", proto["blocking_reasons"])
        self.assertIn("insufficient_reselected_strategy_fills", proto["blocking_reasons"])

    def test_selected_score_does_not_hide_bad_reference_windows_via_abstention(self):
        records = [record(0, .8, 0), record(300, .2, 1)]
        predictions = {r["slug"]: dict(identity=r["p"], market_shrink=.5) for r in records}
        fixed = {records[0]["slug"]: "up", records[1]["slug"]: "down"}
        review, pairs = c.paired_review(records, predictions, fixed, "market_shrink", [])
        self.assertEqual(review["n"], 2)
        self.assertEqual(review["calibrated"]["brier"], .25)
        self.assertAlmostEqual(review["raw"]["brier"], .64)

    def test_cli_refuses_to_overwrite_existing_results(self):
        with tempfile.TemporaryDirectory() as folder:
            marker = Path(folder)/"report.json"
            marker.write_text("preserve existing analysis")
            result = subprocess.run([sys.executable, c.__file__, "--input", "absent.ndjson",
                                     "--output", folder], capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertIn("NEW output directory", result.stderr)
            self.assertEqual(marker.read_text(), "preserve existing analysis")

    def test_cli_rejects_changed_base_model_before_loading_data(self):
        with tempfile.TemporaryDirectory() as folder:
            model = Path(folder)/"model.json"
            model.write_text("{}")
            output = Path(folder)/"new-results"
            result = subprocess.run([sys.executable, c.__file__, "--input", "absent.ndjson",
                                     "--model", str(model), "--output", str(output)],
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertIn("base model hash mismatch", result.stderr)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
