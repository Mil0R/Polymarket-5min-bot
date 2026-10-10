"""Read-only SDDFV capture quality; never repairs data or changes a trading gate."""
import collections
import datetime as dt
import hashlib
import json
import math
from pathlib import Path

CAPTURE_VERSION = 3


def book_topology(pair):
    books = [(pair or {}).get(side, {}) for side in ("up", "down")]
    if not all(b.get("valid") is True for b in books): return "invalid"
    if any(number(b.get("best_bid")) is not None and number(b.get("best_ask")) is not None for b in books): return "two_sided"
    if any(number(b.get(key)) is not None for b in books for key in ("best_bid", "best_ask")): return "single_sided"
    return "empty"


def checkpoint_schedule(events):
    plans, statuses, ended = {}, collections.defaultdict(set), False
    watermark = 0
    for event in events:
        try: watermark = max(watermark, int(dt.datetime.fromisoformat(event.get("recorded_at", "").replace("Z", "+00:00")).timestamp()*1000))
        except (ValueError, TypeError): pass
        ended |= event.get("event") == "run_stopped"
        if event.get("event") != "checkpoint_capture": continue
        key = (event.get("slug"), event.get("checkpoint_secs"))
        status = event.get("status")
        statuses[key].add(status)
        if status == "planned": plans[key] = event
    counts = collections.Counter()
    for key, plan in plans.items():
        due, end = number(plan.get("scheduled_at_ms")), number(plan.get("window_end_ms"))
        if due is None or end is None: counts["invalid_plan"] += 1; continue
        if due > watermark: counts["not_yet_due"] += 1; continue
        counts["due"] += 1
        phases = statuses[key]
        if "captured" in phases: counts["captured"] += 1
        elif "failed" in phases: counts["failed"] += 1
        elif ended or watermark >= end+20000: counts["missed_or_interrupted"] += 1
        else: counts["in_progress_or_snapshot_incomplete"] += 1
    return dict(available=bool(plans), planned=len(plans) if plans else None,
        snapshot_watermark_ms=watermark, counts=dict(counts),
        note="Uses file watermark, not analysis wall-clock; pending/tail windows are not automatically missing.")


def number(value):
    try:
        value = float(value)
        return value if math.isfinite(value) else None
    except (TypeError, ValueError):
        return None


def source_coverage(buckets, end_ms):
    now = number((buckets or {}).get("asof_ms"))
    if now is None:
        return {}
    end = int(end_ms // 1000)
    result = {}
    for source in buckets.get("streams", []):
        seconds = set()
        invalid = 0
        for sample in source.get("samples", []):
            stamp, receive, value = (number(sample.get(k)) for k in ("observed_at_ms", "received_at_ms", "value"))
            if stamp is None or receive is None or value is None or value <= 0 or stamp > now or receive > now:
                invalid += 1
                continue
            seconds.add(int(stamp // 1000))
        missing = [second - (end-60) for second in range(end-60, end) if second not in seconds]
        result[source["topic"]] = dict(expected_seconds=60, observed_seconds=60-len(missing),
            missing_second_offsets=missing, invalid_or_future_samples=invalid,
            proxy_eligible=len(missing)<=6, interpolated=False)
    return result


def review_context(paths, model_path=None):
    plans, modes, diagnostics = [], set(), []
    sha = hashlib.sha256(Path(model_path).read_bytes()).hexdigest() if model_path else None
    for name in paths:
        path = Path(name)
        plan_path = path.parent / "run-plan.json"
        plan = json.loads(plan_path.read_text()) if plan_path.exists() else {}
        plans.append(plan)
        with path.open() as stream:
            for line in stream:
                event = json.loads(line)
                if event.get("event") != "run_started":
                    continue
                explicit = event.get("experiment_mode")
                mode = explicit or plan.get("mode")
                if not mode and event.get("config", {}).get("research_observation_only"):
                    mode = "observation_only_unstaged"
                modes.add(mode or "unknown")
                if explicit and plan.get("mode") and explicit != plan["mode"]:
                    diagnostics.append("run_started_plan_mode_mismatch")
                if sha and (event.get("terminal_model") or {}).get("sha256") != sha:
                    diagnostics.append("run_started_model_hash_mismatch")
    freeze_verified = False
    if model_path:
        root = Path(model_path).parent
        if (root/"freeze.json").exists() and (root/"protocol.json").exists():
            freeze = json.loads((root/"freeze.json").read_text())
            protocol_sha = hashlib.sha256((root/"protocol.json").read_bytes()).hexdigest()
            freeze_verified = (freeze.get("local_validation_passed") is True
                and freeze.get("model_sha256") == sha and freeze.get("protocol_sha256") == protocol_sha
                and all(p.get("mode") == "start" and p.get("model_sha256") == sha
                        and p.get("protocol_sha256") == protocol_sha for p in plans))
    if modes <= {"observe", "observation_only_unstaged"} and modes:
        scope = "observation_only_fixed_model_replay" if model_path else "observation_only"
    elif model_path and modes == {"start"} and freeze_verified and not diagnostics:
        scope = "new_post_freeze_batch"
    else:
        scope = "fixed_model_replay_mode_unverified" if model_path else "historical_development_data_already_reviewed"
    return dict(modes=sorted(modes), review_scope=scope, freeze_evidence_verified=freeze_verified,
        diagnostics=sorted(set(diagnostics)), promotion_scope_verified=scope == "new_post_freeze_batch")


def summarize(paths):
    counts, reasons, baseline, predictions, coverage, connections = (collections.Counter() for _ in range(6))
    counts.update(dict(checkpoints=0,causal_fee_metadata=0,causal_opening_verified=0,
        market_midpoint_available=0,invalid_or_future_bucket_samples=0,missing_or_non_asof_signal_buckets=0))
    by_asset = collections.defaultdict(collections.Counter)
    readiness, topology, capability = (collections.Counter() for _ in range(3))
    scheduling_events = []
    seen = set()
    for name in paths:
        with Path(name).open() as stream:
            for line in stream:
                event = json.loads(line)
                kind = event.get("event")
                if kind in ("checkpoint_capture", "run_stopped") or event.get("recorded_at"):
                    scheduling_events.append({k:event[k] for k in ("event","recorded_at","slug","checkpoint_secs","scheduled_at_ms","window_end_ms","status") if k in event})
                if kind == "reference_feed_connection":
                    connections[(event.get("connection", "unknown"), event.get("phase", "unknown"), event.get("reason", "unknown"))] += 1
                elif kind == "opening_reference_observation":
                    audit = event.get("evidence") or {}
                    reasons[("lookup", audit.get("reason", "unknown"))] += 1
                    for attempt in audit.get("attempts", []):
                        capability[(attempt.get("source","unknown"),str(attempt.get("http_status")),attempt.get("reason","unknown"))] += 1
                elif kind in ("residual_execution_observation", "terminal_reference_window"):
                    if kind == "residual_execution_observation":
                        key = (event["slug"], event["checkpoint_secs"])
                        if key in seen:
                            counts["duplicate_checkpoints"] += 1
                            continue
                        seen.add(key)
                        sig = event["signal"]
                        readiness[(event["asset"],sig.get("model_status","legacy_unspecified"),sig.get("model_unavailable_reason") or "none")] += 1
                        for phase in ("signal", "delayed"):
                            topology[(event["asset"],phase,book_topology(event.get(phase,{}).get("books")))] += 1
                        counts["checkpoints"] += 1
                        by_asset[event["asset"]]["checkpoints"] += 1
                        now = sig["sampled_at_ms"]
                        fees = sig.get("market_fee_audit") or {}
                        stamp = number(fees.get("observed_at_ms"))
                        if fees.get("verified") is True and stamp is not None and 0 <= now-stamp <= 10000:
                            counts["causal_fee_metadata"] += 1
                        opening = sig.get("opening_reference_audit") or {}
                        if opening.get("verified_before_signal") is True and opening.get("within_tolerance") is True:
                            counts["causal_opening_verified"] += 1
                            by_asset[event["asset"]]["causal_opening_verified"] += 1
                        else:
                            reasons[("signal", opening.get("reason", "legacy_missing_or_unverified_opening"))] += 1
                        source = (sig.get("baseline") or {}).get("source", "unknown")
                        baseline[(event["asset"], source)] += 1
                        if source in ("up_midpoint", "down_midpoint"):
                            counts["market_midpoint_available"] += 1
                            by_asset[event["asset"]]["market_midpoint_available"] += 1
                        for candidate, prediction in (sig.get("terminal_model") or {}).get("candidates", {}).items():
                            reason = prediction.get("status", "forecast")
                            if reason == "features_or_horizon_unsupported" and candidate.startswith("market_offset_") and source not in ("up_midpoint", "down_midpoint"):
                                reason = "missing_market_midpoint"
                            predictions[(event["asset"], candidate, reason)] += 1
                        buckets = sig.get("reference_buckets") or {}
                        if buckets.get("asof_ms")!=now:
                            counts["missing_or_non_asof_signal_buckets"] += 1
                            buckets={}
                        end = now
                        phase = "signal_trailing60"
                    else:
                        buckets = event.get("reference_buckets") or {}
                        end = event["window_end_ms"]
                        phase = "terminal_final60"
                    if not buckets:
                        coverage[(event["asset"], phase, "all", "missing_buckets")] += 1
                    for topic, value in source_coverage(buckets, end).items():
                        coverage[(event["asset"], phase, topic, "proxy_eligible" if value["proxy_eligible"] else "insufficient_54_of_60_seconds")] += 1
                        counts["invalid_or_future_bucket_samples"] += value["invalid_or_future_samples"]
    def flatten(counter): return {"/".join(key): value for key, value in sorted(counter.items())}
    return dict(counts=dict(counts), by_asset={a:dict(c) for a,c in sorted(by_asset.items())},
        opening_reasons=flatten(reasons), baseline_sources=flatten(baseline),
        prediction_coverage=flatten(predictions), reference_coverage=flatten(coverage),
        model_readiness=flatten(readiness), book_topology=flatten(topology),
        opening_source_attempts=flatten(capability), checkpoint_schedule=checkpoint_schedule(scheduling_events),
        connection_audit=dict(available=bool(connections), events=flatten(connections)),
        note="Missing evidence is not zero PnL. Coverage does not verify TWAP weights; no interpolation or post-signal repair.")


def apply_context(report, context):
    report["run_context"] = context
    report["review_scope"] = context["review_scope"]
    if not context["promotion_scope_verified"]:
        report["forward_validation_passed"] = False
        if "observation" in context["review_scope"]:
            report["local_validation_passed"] = False
        for group in report.get("groups", {}).values():
            group["review_scope"] = context["review_scope"]
            group["promotion_allowed"] = False
            if "observation" in context["review_scope"]:
                group["local_validation_passed"] = False
                group["reasons"] = sorted(set(group.get("reasons", []) + ["observe_only_not_profit_validation"]))
