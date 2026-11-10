import copy
import unittest

import late_resolution_winner_calibration_audit as audit


def evaluation(key="early", **changes):
    row = dict(evaluation_id=key, asset="btc", slug="btc-window", buy_yes=True,
               evaluated_at="2026-11-03T00:04:30Z", end_date="2026-11-03T00:05:00Z",
               seconds_remaining=30, opening_twap="100", current_twap="101",
               seconds_remaining_bucket="21-40s", model_probability="0.995",
               model_audit=dict(opening_skew_ms=1200, volatility_floor_active=True))
    row.update(changes)
    return row


class AuditTests(unittest.TestCase):
    def test_later_ticks_and_opposite_directions_are_not_independent_samples(self):
        rows = {"early": evaluation(), "late": evaluation("late", buy_yes=False,
                model_probability="0.1", evaluated_at="2026-11-03T00:04:31Z")}
        labels = {"early": dict(won=True, resolved_at="2026-11-03T00:06:00Z"),
                  "late": dict(won=False, resolved_at="2026-11-03T00:06:00Z")}
        result = audit.report(rows, labels)
        self.assertEqual(result["overall"]["n"], 1)
        self.assertEqual(result["overall"]["wins"], 1)

    def test_missing_early_label_does_not_select_a_later_labelled_observation(self):
        rows = {"early": evaluation(), "late": evaluation("late", evaluated_at="2026-11-03T00:04:31Z")}
        result = audit.report(rows, {"late": dict(won=True, resolved_at="2026-11-03T00:06:00Z")})
        self.assertEqual(result["overall"]["n"], 0)
        self.assertEqual(result["missing_selected_labels"], 1)

    def test_premature_and_conflicting_labels_fail(self):
        with self.assertRaises(ValueError):
            audit.report({"early": evaluation()}, {"early": dict(won=True, resolved_at="2026-11-03T00:04:59Z")})
        rows = {"early": evaluation(), "late": evaluation("late", buy_yes=False)}
        with self.assertRaises(ValueError):
            audit.report(rows, {key: dict(won=True, resolved_at="2026-11-03T00:06:00Z") for key in rows})

    def test_only_exact_boundary_twap_can_verify_opening_and_residuals(self):
        rows = {"early": evaluation()}
        labels = {"early": dict(won=True, resolved_at="2026-11-03T00:06:00Z")}
        ref = dict(slug="btc-window", opening_twap="100", closing_twap="102",
                   reference_topic="crypto_prices_twap_sixty", window_start_ms=1000,
                   end_ms=301000, opening_observed_at_ms=1000, closing_observed_at_ms=301000)
        result = audit.report(rows, labels, [ref])
        self.assertEqual(result["opening_reference"]["exact_boundary_verified"], 1)
        self.assertEqual(result["empirical_terminal_residual_bps"]["btc/21-40s"]["0.5"], 100)
        skewed = copy.deepcopy(ref)
        skewed["opening_observed_at_ms"] += 1
        self.assertEqual(audit.report(rows, labels, [skewed])["opening_reference"]["exact_boundary_verified"], 0)

    def test_gamma_metadata_detects_opening_sign_error_without_claiming_raw_capture(self):
        rows = {"early": evaluation(opening_twap="103")}
        labels = {"early": dict(won=True, resolved_at="2026-11-03T00:06:00Z")}
        gamma = {"btc-window": dict(closed=True, fetched_at="2026-11-03T00:06:00Z",
                    description="Chainlink TWAP greater than or equal",
                    eventMetadata=dict(priceToBeat=100, finalPrice=102))}
        result = audit.report(rows, labels, gamma_references=gamma)
        self.assertEqual(result["gamma_metadata_reference"]["n"], 1)
        self.assertEqual(result["gamma_metadata_reference"]["logged_opening_final_sign_vs_label_mismatches"], ["btc-window"])
        self.assertEqual(result["gamma_metadata_reference"]["official_sign_vs_label_mismatches"], [])
        self.assertEqual(result["opening_reference"]["exact_boundary_verified"], 0)

    def test_gamma_cache_needs_explicit_rule_and_post_expiry_closed_metadata(self):
        rows = {"early": evaluation()}
        labels = {"early": dict(won=True, resolved_at="2026-11-03T00:06:00Z")}
        base = dict(closed=True, fetched_at="2026-11-03T00:06:00Z",
                    description="Chainlink TWAP greater than or equal",
                    eventMetadata=dict(priceToBeat=100, finalPrice=102))
        for changes in (dict(closed=False), dict(description="spot"), dict(fetched_at="2026-11-03T00:04:00Z")):
            gamma = {"btc-window": dict(base, **changes)}
            self.assertEqual(audit.report(rows, labels, gamma_references=gamma)["gamma_metadata_reference"]["n"], 0)


if __name__ == "__main__":
    unittest.main()
