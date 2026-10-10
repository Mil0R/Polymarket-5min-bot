import copy
import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

import digital_fair_value_risk as risk
import digital_fair_value_terminal_study as terminal
import digital_fair_value_next_round as workflow
from test_digital_fair_value_terminal_study import rows
from test_digital_fair_value_residual_study import episode, row


def trade(asset="btc",slug="btc-updown-5m-0",entry=270000,end=300000,pnl=-5.0):
    return dict(asset=asset,slug=slug,entry_ms=entry,window_end_ms=end,
                cost=4.7,fees=.3,settlement_pnl=pnl,predicted_probability=.6)


class RiskTests(unittest.TestCase):
    def test_cash_is_locked_until_modeled_release(self):
        ts=[trade(),trade("eth","eth-updown-5m-0",270001)]
        p=dict(initial_balance=5.0,max_exposure=5.0,max_asset_exposure=5.0)
        r=risk.replay(ts,p,asof_ms=300000)
        self.assertEqual(r["fills"],1)
        self.assertEqual(r["cash"],0)
        self.assertEqual(r["locked_cost"],5)
        self.assertEqual(r["realized_pnl"],0)
        self.assertEqual(r["final_settlement_pnl"],-5)
        self.assertEqual(r["rejected"],{"cash_insufficient":1})
        settled=risk.replay(ts,p)
        self.assertEqual(settled["realized_pnl"],-5)

    def test_joint_assets_use_one_budget_not_two_wallets(self):
        ts=[trade(),trade("eth","eth-updown-5m-0",270001)]
        r=risk.replay(ts,dict(initial_balance=100.0,max_exposure=5.0,max_asset_exposure=5.0))
        self.assertEqual(r["fills"],1)
        self.assertEqual(r["rejected"],{"total_exposure_exceeded":1})

    def test_unknown_future_labels_cannot_supply_cash_before_release(self):
        ts=[trade(pnl=100.0),trade("eth","eth-updown-5m-0",270001)]
        p=dict(initial_balance=5.0,max_exposure=5.0,max_asset_exposure=5.0)
        self.assertEqual(risk.replay(ts,p)["fills"],1)
        ts[1]["entry_ms"]=420000
        ts[1]["window_end_ms"]=600000
        self.assertEqual(risk.replay(ts,p)["fills"],2)

    def test_duplicate_markets_and_invalid_budget_rejected(self):
        with self.assertRaisesRegex(ValueError,"duplicate"):
            risk.replay([trade(),trade()])
        for p in (dict(initial_balance=float("nan")),dict(max_exposure=2000.0)):
            with self.assertRaises(ValueError): risk.replay([],p)
        t=trade();t["cost"]=5.0
        with self.assertRaisesRegex(ValueError,"per_order"): risk.replay([t])

    def test_zero_fill_and_partial_dates_do_not_pad_confidence(self):
        value,_,_=risk.summarize([],rows())
        self.assertEqual(value["pnl"],0)
        self.assertFalse(value["full_day_pnl_interval"]["available"])
        self.assertIsNone(value["roi"])
        interval=risk.interval_values([0.0]*5)
        self.assertEqual(interval["lower"],0)

    def test_offset_is_bounded_and_does_not_invent_midpoints(self):
        data=rows()
        source=terminal.fit(data,"conditional_empirical")
        model=terminal.fit_offset(data,source,10)
        for r in data:
            p=terminal.predict(r,model)
            self.assertLessEqual(abs(p-r["market"]),.10000000001)
        r=copy.deepcopy(data[0]);r["market"]=None
        self.assertIsNone(terminal.predict(r,model))
        r=copy.deepcopy(data[0]);r["features"]["vector"]=None
        self.assertIsNone(terminal.predict(r,model))

    def test_next_round_coefficients_ignore_holdout_labels(self):
        data=rows()
        first,_,_=terminal.study(data,[],{},["old"],next_round=True)
        mapping,_=terminal.execution.partition(data)
        changed=copy.deepcopy(data)
        for r in changed:
            if mapping[r["time"]]!="train":
                r["won"]=1-r["won"]
                r["target_bps"]=100000
        second,_,_=terminal.study(changed,[],{},["old"],next_round=True)
        self.assertEqual(first["models"]["btc/30s"]["candidates"],second["models"]["btc/30s"]["candidates"])

    def test_no_selected_candidate_does_not_claim_zero_actual_samples(self):
        _,report,_=terminal.study(rows(),[],{},["old"],next_round=True)
        g=report["groups"]["btc/30s"]
        self.assertIsNone(g["selected"])
        self.assertNotIn("validation_insufficient_paired_samples",g["reasons"])
        self.assertEqual(g["splits"]["validation"]["market_offset_ridge_10"]["paired_model"]["samples"],120)

    def test_missing_causal_fees_fail_closed(self):
        r=rows()[0]
        value=terminal.replay_with_fee(r,.7,dict(terminal.POLICY,fee_source="observed_required"))
        self.assertEqual(value["reason"],"missing_causal_market_fee_schedule")

    def test_causal_fee_and_opening_are_required_and_not_overwritten(self):
        r=row();r["episode"]=episode(r)
        sig=r["episode"]["signal"]
        sig["opening_twap"]=100.0
        sig["market_fee_audit"]=dict(verified=True,slug=r["slug"],observed_at_ms=9500,rate=.02,exponent=1)
        sig["opening_reference_audit"]=dict(official_price_to_beat=100.0,observed_at_ms=9500)
        p=dict(terminal.POLICY,fee_source="observed_required",require_opening_audit=True,failure_rate=0)
        result=terminal.replay_with_fee(r,.8,p)
        self.assertEqual(result["status"],"filled")
        self.assertEqual(result["fee_evidence"],"causal_gamma_schedule")
        self.assertLess(result["fees"],terminal.execution.replay(r,.8,p)["fees"])
        sig["market_fee_audit"]["observed_at_ms"]=10001
        self.assertEqual(terminal.replay_with_fee(r,.8,p)["reason"],"missing_causal_market_fee_schedule")
        sig["market_fee_audit"]["observed_at_ms"]=9500
        sig["opening_reference_audit"]["official_price_to_beat"]=99
        self.assertEqual(terminal.replay_with_fee(r,.8,p)["reason"],"missing_or_mismatched_causal_official_opening")

    def test_absent_buckets_are_unknown_not_zero_model_error(self):
        data=rows(1)
        data[0]["episode"]=dict(signal=dict(sampled_at_ms=100000),window_end_ms=130000)
        result=workflow.reference_audit(data)
        self.assertIsNone(result["proxy_error_mae_bps"])
        self.assertFalse(result["aggregation_rule_verified"])

    def test_failed_local_gate_cannot_freeze_or_start(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            model=dict(schema_version=2,online_trading_enabled=False,policy=dict(terminal.POLICY,**risk.DEFAULTS))
            (root/"model.json").write_text(json.dumps(model))
            sha=workflow.digest(root/"model.json")
            (root/"report.json").write_text(json.dumps(dict(model_sha256=sha,local_validation_passed=False)))
            (root/"protocol.json").write_text(json.dumps(dict(model_sha256=sha,assets=["btc","eth"],
                execution_policy=model["policy"],max_duration_days=14)))
            (root/"parity-report.json").write_text(json.dumps(dict(model_sha256=sha,passed=True,forecasts=1)))
            with self.assertRaisesRegex(ValueError,"local_validation_failed"):
                workflow.freeze(root)
            self.assertFalse((root/"freeze.json").exists())
            with self.assertRaisesRegex(ValueError,"start_blocked"):
                workflow.preflight(root,root/"new-batch"/"events.ndjson","start")
            self.assertFalse((root/"new-batch").exists())

    def test_observe_deadline_does_not_restart_and_expiration_blocks(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            p=dict(terminal.POLICY,**risk.DEFAULTS)
            model=dict(schema_version=2,online_trading_enabled=False,policy=p)
            (root/"model.json").write_text(json.dumps(model))
            sha=workflow.digest(root/"model.json")
            (root/"protocol.json").write_text(json.dumps(dict(model_sha256=sha,assets=["btc","eth"],execution_policy=p,max_duration_days=14)))
            (root/"parity-report.json").write_text(json.dumps(dict(model_sha256=sha,passed=True,forecasts=1)))
            journal=root/"batch"/"events.ndjson"
            with contextlib.redirect_stdout(io.StringIO()):
                workflow.preflight(root,journal,"observe")
            manifest_path=journal.parent/"run-plan.json"
            first=json.loads(manifest_path.read_text())
            with contextlib.redirect_stdout(io.StringIO()):
                workflow.preflight(root,journal,"observe")
            self.assertEqual(first,json.loads(manifest_path.read_text()))
            first["deadline_ms"]=1
            manifest_path.write_text(json.dumps(first))
            with self.assertRaisesRegex(ValueError,"deadline_reached"):
                workflow.preflight(root,journal,"observe")


if __name__=="__main__":
    unittest.main()
