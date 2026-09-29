import copy
import json
import tempfile
import unittest
from pathlib import Path

import digital_fair_value_residual_study as s


def row(index=1, won=1):
    return dict(slug=f"btc-updown-5m-{index*300}", checkpoint=30, time=index*300,
                asset="btc", market=0.5, gaussian=0.7, margin=1.0, won=won,
                source="up_midpoint", episode=None)


def episode(r):
    def book(now, price=0.5, size=100):
        return dict(valid=True, asks=[dict(price=price, size=size)],
                    http=dict(exchange_timestamp=now-20, rest_latency_ms=30))
    audit = dict(observation_age_ms=500, local_receive_age_ms=100, network_receive_latency_ms=10)
    return dict(episode_id="test-no-failure", window_end_ms=30000,
                signal=dict(sampled_at_ms=10000, model_audit=audit,
                            books=dict(up=book(10000), down=book(10000))),
                delayed=dict(sampled_at_ms=11030, actual_delay_ms=1030,
                             monotonic_delay_ms=1030, before_expiry=True,
                             books=dict(up=book(11030), down=book(11030))))


class ResidualTests(unittest.TestCase):
    def test_atomic_time_partition_and_train_only_fit(self):
        rows = [dict(row(i, i % 2), gaussian=0.8 if i % 2 else 0.2) for i in range(1, 151)]
        rows += [dict(r, asset="eth") for r in rows]
        mapping, _ = s.partition(rows)
        self.assertEqual(mapping[90*300], "train")
        self.assertEqual(mapping[91*300], "validation")
        self.assertEqual(mapping[121*300], "holdout")
        artifact, _, _ = s.study(rows, [], {})
        self.assertNotEqual(artifact["assets"]["btc"]["selection"], "market_baseline_no_validated_residual")
        changed = copy.deepcopy(rows)
        for r in changed:
            if mapping[r["time"]] == "holdout":
                r["won"] = 1-r["won"]
                r["gaussian"] = 0.99
                r["margin"] = -90
        second, _, _ = s.study(changed, [], {})
        self.assertEqual(artifact["assets"], second["assets"])

    def test_fit_signal_and_capped_correction(self):
        rows = [dict(row(i), gaussian=0.8 if i % 2 else 0.2, won=i % 2) for i in range(1, 150)]
        model = s.fit(rows, 1)
        self.assertGreater(s.predict(rows[0], model), 0.5)
        for r in rows:
            self.assertLessEqual(abs(s.predict(r, model)-r["market"]), 0.10000001)

    def test_legacy_cannot_pass_profit_gate(self):
        rows = [row(i, i % 2) for i in range(1, 151)]
        _, report, trades = s.study(rows, [], {})
        self.assertFalse(report["assets"]["btc"]["promotion_allowed"])
        self.assertFalse(trades)
        self.assertIsNone(report["assets"]["btc"]["executions"]["holdout"]["residual"]["settlement_pnl"])
        self.assertEqual(s.replay(row(), 0.8, s.POLICY)["reason"], "missing_linked_delayed_book")

    def test_frozen_validation_rejects_overlap_and_wrong_checkpoint(self):
        rows = [row(i) for i in range(1, 151)]
        model, _, _ = s.study(rows, [], {})
        with self.assertRaisesRegex(ValueError, "overlaps"):
            s.study(rows, [], {}, model)
        with self.assertRaisesRegex(ValueError, "checkpoint"):
            s.study([dict(row(200), checkpoint=10)], [], {}, model)

    def test_cost_depth_and_settlement_pnl(self):
        r = row()
        r["episode"] = episode(r)
        p = dict(s.POLICY, failure_rate=0)
        fill = s.replay(r, 0.8, p)
        self.assertEqual(fill["status"], "filled")
        self.assertAlmostEqual(fill["cost"]+fill["fees"], 5)
        self.assertGreater(fill["fees"], 0)
        self.assertGreater(fill["average_price"], 0.5)
        self.assertAlmostEqual(fill["settlement_pnl"], fill["shares"]-5)
        r["won"] = 0
        self.assertAlmostEqual(s.replay(r, 0.8, p)["settlement_pnl"], -5)
        r["episode"]["delayed"]["books"]["up"]["asks"][0]["size"] = 4
        fill = s.replay(r, 0.8, p)
        self.assertEqual(fill["status"], "filled")
        self.assertLess(fill["partial_fill_ratio"], 1)
        r["episode"]["delayed"]["books"]["up"]["asks"][0]["size"] = 1
        self.assertEqual(s.replay(r, 0.8, p)["status"], "rejected")

    def test_delay_and_stale_quality_fail_closed(self):
        for key, value in [("actual_delay_ms", 500), ("monotonic_delay_ms", 900), ("before_expiry", False)]:
            r = row()
            r["episode"] = episode(r)
            r["episode"]["delayed"][key] = value
            self.assertEqual(s.replay(r, 0.8, s.POLICY)["status"], "excluded")
        r = row()
        r["episode"] = episode(r)
        r["episode"]["signal"]["model_audit"]["network_receive_latency_ms"] = 4000
        self.assertEqual(s.replay(r, 0.8, s.POLICY)["status"], "excluded")
        r["episode"] = episode(r)
        r["episode"]["delayed"]["books"]["up"]["http"]["exchange_timestamp"] -= 4000
        self.assertEqual(s.replay(r, 0.8, s.POLICY)["status"], "excluded")

    def test_delay_does_not_reforecast_and_failures_have_no_fills(self):
        r = row()
        r["episode"] = episode(r)
        p = dict(s.POLICY, failure_rate=0)
        r["episode"]["delayed"]["probability_up"] = 0.999
        r["episode"]["delayed"]["books"]["up"]["asks"][0]["price"] = 0.9
        self.assertEqual(s.replay(r, 0.8, p)["status"], "rejected")
        r["episode"] = episode(r)
        self.assertEqual(s.replay(r, 0.8, dict(p, failure_rate=1))["reason"], "simulated_order_failure")

    def test_interval_is_excluded_from_prediction_gate(self):
        rows = [dict(row(i), source="complementary_ask_interval") for i in range(1, 200)]
        _, report, _ = s.study(rows, [], {})
        btc = report["assets"]["btc"]
        self.assertFalse(btc["promotion_allowed"])
        self.assertEqual(btc["interval_baseline_samples_excluded"], 199)
        self.assertTrue(all(v["market"]["samples"] == 0 for v in btc["paired_predictions"].values()))

    def test_loader_keeps_signal_episode_not_later_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/"events.ndjson"
            slug = "btc-updown-5m-300"
            observation = dict(event="model_observation", slug=slug, asset="btc", checkpoint_secs=30,
                               market_probability_up=0.9, probability_up=0.99,
                               model_audit=dict(directional_margin_bps=1))
            ep = episode(row())
            ep.update(event="residual_execution_observation", slug=slug, asset="btc", checkpoint_secs=30)
            ep["signal"].update(probability_up=0.7, baseline=dict(probability_up=0.5, source="up_midpoint"))
            ep["signal"]["model_audit"]["directional_margin_bps"] = 1
            path.write_text(json.dumps(observation)+"\n"+json.dumps(ep)+"\n")
            (Path(directory)/"reference-audit.json").write_text(json.dumps({slug: dict(winner_up=True, confirmation="closed_terminal_prices")}))
            rows, _, _ = s.load([path])
            self.assertEqual(len(rows), 1)
            self.assertEqual(rows[0]["market"], 0.5)


if __name__ == "__main__":
    unittest.main()
