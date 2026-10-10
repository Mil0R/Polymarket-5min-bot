import copy
import math
import json
import tempfile
import unittest
from pathlib import Path

import digital_fair_value_terminal_study as s


def signal(age_ms=2000):
    return dict(sampled_at_ms=100000, seconds_remaining=30.0,
                opening_twap=100.0, current_twap=100.01,
                model_audit=dict(reference_observed_at_ms=100000-age_ms),
                twap_anchors=dict(anchors=[dict(ago_ms=h*1000, observed_at_ms=100000-age_ms-h*1000, value=100.0)
                                          for h in (30, 60)]),
                spot_asof=dict(streams=[dict(topic="crypto_prices_chainlink", sample=dict(
                    observed_at_ms=99500, received_at_ms=99900, value=100.02))]))


def rows(n=600):
    result = []
    for i in range(n):
        sig = signal()
        sig["current_twap"] = 100.0+(i % 7-3)*0.005
        f = s.signal_features(sig, 130000)
        target = 0.6*f["vector"][0]+0.4*f["vector"][2]+(i % 5-2)*0.15
        result.append(dict(slug=f"btc-updown-5m-{i*300}", asset="btc", checkpoint=30,
                           time=i*300, features=f, target_bps=target, market=0.5,
                           gaussian=0.5, won=int(f["margin_bps"]+target>=0), episode=None))
    return result


class TerminalTests(unittest.TestCase):
    def test_journal_loader_excludes_late_checkpoint_captures(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            sig=signal()
            sig["sampled_at_ms"]+=170000
            sig["model_audit"]["reference_observed_at_ms"]+=170000
            sig["twap_anchors"]["topic"]="crypto_prices_twap_sixty"
            for a in sig["twap_anchors"]["anchors"]:
                a["observed_at_ms"]+=170000
            spot=sig["spot_asof"]["streams"][0]["sample"]
            spot["observed_at_ms"]+=170000
            spot["received_at_ms"]+=170000
            sig["probability_up"]=0.5
            event=dict(event="residual_execution_observation", slug="btc-updown-5m-0", checkpoint_secs=30,
                       episode_id="fixture",asset="btc",window_end_ms=300000,signal=sig)
            (root/"reference-audit.json").write_text(json.dumps({event["slug"]:dict(
                winner_up=True,confirmation="closed_terminal_prices",price_to_beat=100.0,final_price=100.02)}))
            path=root/"events.ndjson"
            path.write_text(json.dumps(event)+"\n")
            loaded,_,_,_=s.load([path])
            self.assertEqual(len(loaded),1)
            self.assertEqual(loaded[0]["features"]["horizon_secs"],32)
            sig["sampled_at_ms"]+=15000
            sig["model_audit"]["reference_observed_at_ms"]+=15000
            sig["seconds_remaining"]=15.0
            path.write_text(json.dumps(event)+"\n")
            loaded,diagnostics,_,_=s.load([path])
            self.assertFalse(loaded)
            self.assertEqual(diagnostics["capture_late_for_checkpoint"],1)

    def test_horizon_includes_reference_age_without_sqrt_extrapolation(self):
        f = s.signal_features(signal(), 130000)
        self.assertEqual(f["horizon_secs"], 32)
        self.assertEqual(f["reference_age_secs"], 2)
        model = s.fit(rows(), "asset_gaussian")
        r = rows()[0]
        old = s.predict(r, model)
        r["features"]["horizon_secs"] = 32.5
        self.assertEqual(old, s.predict(r, model))
        r["features"]["horizon_secs"] = 60
        self.assertIsNone(s.predict(r, model))

    def test_future_spot_or_anchors_never_become_training_features(self):
        for which in ("spot", "anchor"):
            sig = signal()
            if which == "spot":
                sig["spot_asof"]["streams"][0]["sample"]["received_at_ms"] = 100001
            else:
                sig["twap_anchors"]["anchors"][0]["observed_at_ms"] = 100001
            self.assertIsNone(s.signal_features(sig, 130000)["vector"])
        sig = signal()
        sig["seconds_remaining"] = 29
        with self.assertRaisesRegex(ValueError, "time_mismatch"):
            s.signal_features(sig, 130000)

    def test_missing_features_do_not_become_zero_price(self):
        sig = signal()
        sig["spot_asof"] = {}
        f = s.signal_features(sig, 130000)
        self.assertIsNone(f["vector"])
        r = dict(rows()[0], features=f)
        self.assertIsNotNone(s.predict(r, s.fit(rows(), "asset_gaussian")))
        self.assertIsNone(s.predict(r, s.fit(rows(), "conditional_gaussian")))

    def test_training_scale_and_conditional_location(self):
        data = rows()
        model = s.fit(data, "conditional_gaussian")
        baseline = s.fit(data, "asset_gaussian")
        error = sum((r["target_bps"]-s.location(r, model))**2 for r in data)
        base_error = sum((r["target_bps"]-s.location(r, baseline))**2 for r in data)
        self.assertLess(error, base_error)
        self.assertGreater(model["sigma_floor_bps"], 0)
        self.assertTrue(all(s.scale(r, model)>=model["sigma_floor_bps"] for r in data))

    def test_models_never_fit_validation_or_review_labels(self):
        data = rows()
        first, _, _ = s.study(data, [], {}, ["old-run"])
        changed = copy.deepcopy(data)
        mapping, _ = s.execution.partition(changed)
        for r in changed:
            if mapping[r["time"]] != "train":
                r["target_bps"] = 1e6
                r["won"] = 1-r["won"]
        second, _, _ = s.study(changed, [], {}, ["old-run"])
        self.assertEqual(first["models"]["btc/30s"]["candidates"], second["models"]["btc/30s"]["candidates"])

    def test_fresh_batch_cannot_reuse_old_run_or_refit(self):
        model, _, _ = s.study(rows(), [], {}, ["old-run"])
        new = rows(200)
        for r in new:
            r["time"] += 300000
            r["slug"] = f"btc-updown-5m-{r['time']}"
        with self.assertRaisesRegex(ValueError, "new_later_batch"):
            s.study(new, [], {}, ["old-run"], model)
        artifact, report, _ = s.study(new, [], {}, ["new-run"], model)
        self.assertEqual(artifact, model)
        self.assertEqual(report["groups"]["btc/30s"]["review_scope"], "new_post_freeze_batch")

    def test_empirical_equality_counts_up_and_limits_tail_confidence(self):
        model = s.fit(rows(), "conditional_empirical")
        r = rows()[0]
        r["features"]["margin_bps"] = 1e6
        self.assertLess(s.predict(r, model), 1)
        r["features"]["margin_bps"] = -1e6
        self.assertGreater(s.predict(r, model), 0)

    def test_fresh_online_predictions_must_match_frozen_model(self):
        artifact, _, _ = s.study(rows(), [], {}, ["old-run"])
        r = rows()[0]
        candidates = artifact["models"]["btc/30s"]["candidates"]
        audit = dict(sha256="abc", decision_role="research_only", candidates={
            name:dict(probability_up=s.predict(r,m)) for name,m in candidates.items()})
        r["episode"] = dict(signal=dict(terminal_model=audit))
        self.assertTrue(s.check_online_predictions([r],artifact,"abc")["passed"])
        audit["candidates"]["asset_gaussian"]["probability_up"] = 0.123
        with self.assertRaisesRegex(ValueError, "probability_mismatch"):
            s.check_online_predictions([r],artifact,"abc")


if __name__ == "__main__":
    unittest.main()
