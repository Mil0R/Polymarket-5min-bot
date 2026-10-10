import copy
import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import digital_fair_value_observation_audit as audit
import digital_fair_value_next_round as workflow
import digital_fair_value_risk as risk
import digital_fair_value_terminal_study as terminal
from test_digital_fair_value_residual_study import episode, row


class ObservationAuditTests(unittest.TestCase):
    def test_capture_schedule_preserves_tail_inflight_and_failures(self):
        def plan(slug,due,end):
            return dict(event="checkpoint_capture",slug=slug,checkpoint_secs=30,
                status="planned",scheduled_at_ms=due,window_end_ms=end,recorded_at="1970-01-01T00:00:01Z")
        events=[plan("old",1000,30000),plan("tail",100000,130000),plan("inflight",59900,90000),
            plan("failed",1000,30000),plan("captured",1000,30000),
            dict(event="checkpoint_capture",slug="failed",checkpoint_secs=30,status="failed"),
            dict(event="checkpoint_capture",slug="captured",checkpoint_secs=30,status="captured"),
            dict(event="terminal_reference_window",recorded_at="1970-01-01T00:01:00Z")]
        value=audit.checkpoint_schedule(events)
        self.assertEqual(value["counts"],dict(due=4,missed_or_interrupted=1,not_yet_due=1,
            in_progress_or_snapshot_incomplete=1,failed=1,captured=1))
        self.assertIsNone(audit.checkpoint_schedule([])["planned"])

    def test_single_sided_books_are_not_imputed(self):
        pair=dict(up=dict(valid=True,best_ask=.9),down=dict(valid=True,best_bid=.08))
        self.assertEqual(audit.book_topology(pair),"single_sided")
        self.assertNotIn("best_bid",pair["up"])
        pair["down"]["best_ask"]=.1
        self.assertEqual(audit.book_topology(pair),"two_sided")
        pair["up"]["valid"]=False
        self.assertEqual(audit.book_topology(pair),"invalid")

    def test_qualified_page_provenance_is_required(self):
        r=row();r["episode"]=episode(r);sig=r["episode"]["signal"]
        r["episode"]["window_end_ms"]=300000
        sig["sampled_at_ms"]=270000;sig["opening_twap"]=100
        sig["books"]["up"]["http"]["exchange_timestamp"]=269980
        sig["books"]["down"]["http"]["exchange_timestamp"]=269980
        delayed=r["episode"]["delayed"];delayed["sampled_at_ms"]=271030
        for b in delayed["books"].values():b["http"]["exchange_timestamp"]=271010
        sig["market_fee_audit"]=dict(verified=True,slug=r["slug"],condition_id="test",observed_at_ms=269500,rate=.02,exponent=1)
        sig["opening_reference_audit"]=dict(schema_version=3,verified=True,verified_before_signal=True,
            source="polymarket_event_page",official_price_to_beat=100,observed_at_ms=269500,
            slug=r["slug"],condition_id="test",window_end_ms=300000,reason="verified_live_opening",
            source_data_updated_at_ms=1000,reference_rule="chainlink_twap_sixty",response_sha256="a"*64,
            query_key=["crypto-prices","price","BTC","1970-01-01T00:00:00Z","fiveminute","1970-01-01T00:05:00Z",True,60])
        policy=dict(terminal.POLICY,require_opening_audit=True,fee_source="observed_required",failure_rate=0)
        self.assertEqual(terminal.replay_with_fee(r,.8,policy)["status"],"filled")
        for key,value in [("source_data_updated_at_ms",270001),("response_sha256","z"*64),
                ("query_key",None),("reference_rule","spot")]:
            changed=copy.deepcopy(r);changed["episode"]["signal"]["opening_reference_audit"][key]=value
            self.assertEqual(terminal.replay_with_fee(changed,.8,policy)["reason"],"unverified_page_opening_provenance")
        changed=copy.deepcopy(r);changed["episode"]["signal"]["opening_reference_audit"]["query_key"][6]=1
        self.assertEqual(terminal.replay_with_fee(changed,.8,policy)["reason"],"unverified_page_opening_provenance")
        changed=copy.deepcopy(r);changed["episode"]["signal"]["opening_reference_audit"]["source"]="unreviewed_scraper"
        self.assertEqual(terminal.replay_with_fee(changed,.8,policy)["reason"],"unverified_opening_source")

    def test_unready_raw_capture_cannot_become_a_model_row(self):
        with tempfile.TemporaryDirectory() as name:
            root=Path(name);path=root/"events.ndjson"
            (root/"reference-audit.json").write_text("{}")
            raw=dict(event="residual_execution_observation",slug="btc-updown-5m-0",checkpoint_secs=30,
                episode_id="unready",asset="btc",window_end_ms=300000,
                signal=dict(model_status="unavailable",model_unavailable_reason="opening_sample_missing",probability_up=None))
            path.write_text(json.dumps(dict(event="run_started",run_id="new"))+"\n"+json.dumps(raw)+"\n")
            rows,diagnostics,inputs,run_ids=terminal.load([path])
            self.assertFalse(rows)
            self.assertEqual(diagnostics["model_unavailable/opening_sample_missing"],1)
            frozen=dict(schema_version=2,model_type="sddfv_terminal_residual_v2",split_boundaries=dict(data_end=0),
                policy=dict(terminal.POLICY,**risk.DEFAULTS),models={},run_ids=["old"])
            artifact,report,trades=terminal.study(rows,inputs,diagnostics,run_ids,frozen)
            self.assertIs(artifact,frozen)
            self.assertFalse(report["forward_validation_passed"])
            self.assertFalse(trades)
            self.assertIn("PnL is unavailable",report["limitations"][0])

    def test_all_unready_batch_still_produces_diagnostic_reports(self):
        with tempfile.TemporaryDirectory() as name:
            root=Path(name);path=root/"events.ndjson";source=root/"fixed.json";output=root/"analysis"
            frozen=dict(schema_version=2,model_type="sddfv_terminal_residual_v2",split_boundaries=dict(data_end=0),
                policy=dict(terminal.POLICY,**risk.DEFAULTS),models={},run_ids=["old"],online_trading_enabled=False)
            source.write_text(json.dumps(frozen,sort_keys=True,indent=2)+"\n")
            (root/"reference-audit.json").write_text("{}")
            raw=dict(event="residual_execution_observation",slug="btc-updown-5m-0",checkpoint_secs=30,
                episode_id="unready",asset="btc",window_end_ms=300000,
                signal=dict(sampled_at_ms=270000,model_status="unavailable",model_unavailable_reason="opening_sample_missing",probability_up=None))
            path.write_text(json.dumps(dict(event="run_started",run_id="new",experiment_mode="observe",
                terminal_model=dict(sha256=workflow.digest(source)),config=dict(research_observation_only=True)))+"\n"+json.dumps(raw)+"\n")
            subprocess.run([sys.executable,str(Path(terminal.__file__)),"--input",str(path),"--model",str(source),
                "--symbols","btc,eth","--output",str(output)],check=True,capture_output=True,
                env=dict(os.environ,PYTHONDONTWRITEBYTECODE="1"))
            report=json.loads((output/"report.json").read_text())
            self.assertFalse(report["online_inference_audit"]["passed"])
            with contextlib.redirect_stdout(io.StringIO()):overview=workflow.audit(output,[path])
            self.assertFalse(overview["forward_validation_passed"])
            self.assertEqual(overview["capture_audit"]["counts"]["checkpoints"],1)
            self.assertFalse(overview["portfolio_scenarios"])

    def test_future_buckets_excluded_without_interpolation(self):
        samples=[dict(observed_at_ms=i*1000,received_at_ms=i*1000+1,value=100) for i in range(60)]
        samples[-1]["received_at_ms"]=60001
        result=audit.source_coverage(dict(asof_ms=60000,streams=[dict(topic="test",samples=samples)]),60000)["test"]
        self.assertEqual(result["observed_seconds"],59)
        self.assertEqual(result["invalid_or_future_samples"],1)
        self.assertEqual(result["missing_second_offsets"],[59])
        self.assertFalse(result["interpolated"])

    def test_observe_plan_is_not_post_freeze_even_with_model(self):
        with tempfile.TemporaryDirectory() as name:
            root=Path(name); model=root/"model.json";model.write_text("{}")
            sha=workflow.digest(model)
            journal=root/"events.ndjson"
            journal.write_text(json.dumps(dict(event="run_started",terminal_model=dict(sha256=sha),
                config=dict(research_observation_only=True)))+"\n")
            (root/"run-plan.json").write_text(json.dumps(dict(mode="observe")))
            context=audit.review_context([journal],model)
            self.assertEqual(context["review_scope"],"observation_only_fixed_model_replay")
            report=dict(forward_validation_passed=True,local_validation_passed=True,
                groups=dict(btc=dict(promotion_allowed=True,local_validation_passed=True,reasons=[])))
            audit.apply_context(report,context)
            self.assertFalse(report["forward_validation_passed"])
            self.assertFalse(report["local_validation_passed"])
            self.assertFalse(report["groups"]["btc"]["promotion_allowed"])

    def test_start_without_freeze_and_conflicting_modes_do_not_promote(self):
        with tempfile.TemporaryDirectory() as name:
            root=Path(name);model=root/"model.json";model.write_text("{}")
            journal=root/"events.ndjson"
            journal.write_text(json.dumps(dict(event="run_started",experiment_mode="start"))+"\n")
            (root/"run-plan.json").write_text(json.dumps(dict(mode="start")))
            self.assertFalse(audit.review_context([journal],model)["promotion_scope_verified"])
            (root/"run-plan.json").write_text(json.dumps(dict(mode="observe")))
            self.assertIn("run_started_plan_mode_mismatch",audit.review_context([journal],model)["diagnostics"])

    def test_genuine_start_requires_matching_freeze_model_and_protocol(self):
        with tempfile.TemporaryDirectory() as name:
            root=Path(name);model=root/"model.json";model.write_text("{}")
            protocol=root/"protocol.json";protocol.write_text("{}")
            sha=workflow.digest(model); protocol_sha=workflow.digest(protocol)
            (root/"freeze.json").write_text(json.dumps(dict(local_validation_passed=True,
                model_sha256=sha,protocol_sha256=protocol_sha)))
            journal=root/"events.ndjson"
            journal.write_text(json.dumps(dict(event="run_started",experiment_mode="start",
                terminal_model=dict(sha256=sha)))+"\n")
            plan=root/"run-plan.json"
            plan.write_text(json.dumps(dict(mode="start",model_sha256=sha,protocol_sha256=protocol_sha)))
            self.assertTrue(audit.review_context([journal],model)["promotion_scope_verified"])
            plan.write_text(json.dumps(dict(mode="start",model_sha256=sha,protocol_sha256="wrong")))
            self.assertFalse(audit.review_context([journal],model)["promotion_scope_verified"])

    def test_old_capture_plan_cannot_be_silently_appended(self):
        with tempfile.TemporaryDirectory() as name:
            root=Path(name);p=dict(terminal.POLICY,**risk.DEFAULTS)
            (root/"model.json").write_text(json.dumps(dict(schema_version=2,online_trading_enabled=False,policy=p)))
            sha=workflow.digest(root/"model.json")
            (root/"protocol.json").write_text(json.dumps(dict(model_sha256=sha,assets=["btc","eth"],execution_policy=p,max_duration_days=14)))
            (root/"parity-report.json").write_text(json.dumps(dict(model_sha256=sha,passed=True,forecasts=1)))
            journal=root/"new"/"events.ndjson"
            with contextlib.redirect_stdout(io.StringIO()):workflow.preflight(root,journal,"observe")
            plan=journal.parent/"run-plan.json";value=json.loads(plan.read_text());value.pop("audit_capture_version")
            plan.write_text(json.dumps(value)); before=plan.read_bytes()
            with self.assertRaisesRegex(ValueError,"run_plan_changed"):workflow.preflight(root,journal,"observe")
            self.assertEqual(plan.read_bytes(),before)

    def test_new_opening_schema_cannot_bypass_identity_or_verified_flags(self):
        r=row();r["episode"]=episode(r);sig=r["episode"]["signal"]
        sig["opening_twap"]=100
        sig["market_fee_audit"]=dict(verified=True,slug=r["slug"],condition_id="test",observed_at_ms=9500,rate=.02,exponent=1)
        sig["opening_reference_audit"]=dict(schema_version=2,verified=True,verified_before_signal=True,
            official_price_to_beat=100,observed_at_ms=9500,slug=r["slug"],condition_id="test",
            window_end_ms=r["episode"]["window_end_ms"],reason="verified_live_opening")
        policy=dict(terminal.POLICY,require_opening_audit=True,fee_source="observed_required",failure_rate=0)
        self.assertEqual(terminal.replay_with_fee(r,.8,policy)["status"],"filled")
        for key,value in [("verified",False),("condition_id","wrong"),("reason","opening_value_conflict")]:
            changed=copy.deepcopy(r);changed["episode"]["signal"]["opening_reference_audit"][key]=value
            self.assertEqual(terminal.replay_with_fee(changed,.8,policy)["reason"],"unverified_or_identity_mismatched_live_opening")


if __name__=="__main__":unittest.main()
