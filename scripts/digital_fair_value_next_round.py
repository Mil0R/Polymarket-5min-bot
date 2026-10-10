#!/usr/bin/env python3
"""SDDFV 009 staged workflow. A failed gate never enables a trading process."""
import argparse
import collections
import datetime as dt
import hashlib
import json
import math
from pathlib import Path
import subprocess
import sys

import digital_fair_value_terminal_study as terminal
import digital_fair_value_risk as risk
import digital_fair_value_observation_audit as capture_audit


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def reference_audit(rows):
    counts=collections.Counter()
    errors=[]
    known_seconds=[]
    for row in rows:
        sig=row["episode"]["signal"]
        buckets=sig.get("reference_buckets") or {}
        now=sig["sampled_at_ms"]
        end=row["episode"]["window_end_ms"]
        counts["episodes"]+=1
        fees=sig.get("market_fee_audit") or {}
        if fees.get("verified") is True and terminal.number(fees.get("observed_at_ms")) is not None and fees["observed_at_ms"]<=now:
            counts["causal_fee_metadata"]+=1
        opening=sig.get("opening_reference_audit") or {}
        if opening.get("verified_before_signal") is True and opening.get("within_tolerance") is True:
            counts["opening_verified_before_signal"]+=1
        if buckets.get("asof_ms")!=now:
            counts["missing_asof_reference_buckets"]+=1
            continue
        streams={s["topic"]:s.get("samples",[]) for s in buckets.get("streams",[])}
        spot={}
        for sample in streams.get("crypto_prices_chainlink",[]):
            stamp,receive,price=(terminal.number(sample.get(k)) for k in ("observed_at_ms","received_at_ms","value"))
            if stamp is None or receive is None or price is None or price<=0 or stamp>now or receive>now:
                counts["invalid_or_future_bucket"]+=1
                continue
            spot[int(stamp//1000)]=price
        observed=sig.get("model_audit",{}).get("reference_observed_at_ms")
        if observed is None:
            continue
        end_second=int(observed//1000)
        values=[spot[s] for s in range(end_second-60,end_second) if s in spot]
        if len(values)>=54:
            # Diagnostic candidate reconstruction; weighting rule is NOT
            # verified by agreement, and this is never a forecast feature.
            rebuilt=sum(values)/len(values)
            error=terminal.log_bps(rebuilt,sig["current_twap"])
            errors.append(abs(error))
            counts["rolling_60s_proxy_comparable"]+=1
        prefix=[s for s in spot if end//1000-60<=s<min(end//1000,now//1000)]
        known_seconds.append(len(prefix))
    return dict(counts=dict(counts), aggregation_rule_verified=False,
                proxy_error_mae_bps=sum(errors)/len(errors) if errors else None,
                proxy_error_p95_bps=terminal.percentile(errors,.95) if errors else None,
                mean_observed_final_window_seconds=sum(known_seconds)/len(known_seconds) if known_seconds else None,
                decision_role="research_only",
                status="needs_official_weighting_confirmation_and_new_bucket_data",
                note="Bucket coverage is not official weighting. Missing seconds are not filled with zero or future prices.")


def terminal_reference_audit(paths):
    counts=collections.Counter()
    errors=[]
    seen=set()
    for name in paths:
        path=Path(name)
        labels=json.loads((path.parent/"reference-audit.json").read_text())
        with path.open() as stream:
            for line in stream:
                event=json.loads(line)
                if event.get("event")!="terminal_reference_window" or event.get("asset") not in ("btc","eth"):
                    continue
                slug=event["slug"]
                if slug in seen: continue
                seen.add(slug)
                counts["post_signal_terminal_windows"]+=1
                if event.get("decision_role")!="post_signal_audit_only":
                    raise ValueError("terminal_reference_window_role_mismatch")
                buckets=event.get("reference_buckets") or {}
                now=buckets.get("asof_ms",0)
                end=event["window_end_ms"]//1000
                spot={}
                for source in buckets.get("streams",[]):
                    if source["topic"]!="crypto_prices_chainlink": continue
                    for sample in source.get("samples",[]):
                        stamp,receive,value=(terminal.number(sample.get(k)) for k in ("observed_at_ms","received_at_ms","value"))
                        if stamp is not None and receive is not None and value is not None and value>0 and stamp<=now and receive<=now:
                            spot[int(stamp//1000)]=value
                prices=[spot[s] for s in range(end-60,end) if s in spot]
                label=labels.get(slug,{})
                if len(prices)<54:
                    counts["insufficient_terminal_bucket_coverage"]+=1
                    continue
                if label.get("confirmation")!="closed_terminal_prices":
                    counts["missing_official_final_label"]+=1
                    continue
                error=terminal.log_bps(sum(prices)/len(prices),label.get("final_price"))
                if error is None:
                    counts["missing_official_final_price"]+=1
                    continue
                errors.append(abs(error))
    return dict(counts=dict(counts),proxy_final_error_mae_bps=sum(errors)/len(errors) if errors else None,
                proxy_final_error_p95_bps=terminal.percentile(errors,.95) if errors else None,
                aggregation_rule_verified=False,decision_role="post_signal_audit_only",
                note="These future fragments are label/weighting diagnostics; NEVER inference inputs.")


def audit(directory, paths):
    root=Path(directory)
    model=json.loads((root/"model.json").read_text())
    report=json.loads((root/"report.json").read_text())
    if digest(root/"model.json")!=report["model_sha256"]:
        raise ValueError("model_report_hash_mismatch")
    rows,_,_,_=terminal.load(paths)
    rows=[r for r in rows if r["asset"] in ("btc","eth")]
    mapping=terminal.execution.partition(rows)[0] if rows and not report.get("fixed_model_replay") else {}
    trades=[json.loads(line) for line in (root/"trades.ndjson").read_text().splitlines() if line]
    ledgers=[]
    scenarios={}
    for split in sorted({t["split"] for t in trades}):
        scenarios[split]={}
        for name in sorted({t["model"] for t in trades}):
            subset=[t for t in trades if t["split"]==split and t["model"]==name]
            subrows=[r for r in rows if ("fresh_validation" if report.get("fixed_model_replay") else mapping[r["time"]])==split]
            value,_,events=risk.summarize(subset,subrows,model["policy"])
            scenarios[split][name]=value
            ledgers.extend(dict(event,split=split,scenario=name) for event in events)
    protocol=dict(schema_version=1,strategy="short_duration_digital_fair_value",batch="market-offset-009",
                  model_sha256=report["model_sha256"],assets=["btc","eth"],checkpoint_secs=30,
                  max_duration_days=14,min_complete_days=5,min_independent_fills_per_asset=100,
                  execution_policy=model["policy"],observe_only=True,online_trading_enabled=False,
                  source_end=model["split_boundaries"]["data_end"],
                  stopping_rules=["protocol/model/settings mismatch", "disk guard or integrity failure",
                                  "14-day deadline; insufficient data is inconclusive, never pass",
                                  "final nonpositive net/cost-stress PnL or nonpositive daily-block interval: no promotion"],
                  no_reselection_or_threshold_changes=True)
    context=report.get("run_context",{})
    observe="observation" in report["review_scope"]
    overview=dict(model_sha256=report["model_sha256"],local_validation_passed=report.get("local_validation_passed",False),
                  forward_validation_passed=report.get("forward_validation_passed",False),
                  run_context=context,review_scope=report["review_scope"],capture_audit=capture_audit.summarize(paths),
                  reference_audit=reference_audit(rows),terminal_reference_audit=terminal_reference_audit(paths),portfolio_scenarios=scenarios,
                  stage_status={"1_account_reporting":"implemented_and_tested",
                                "2_twap_components":"audit_capture_implemented; weighting/data_validation_pending",
                                "3_local_validation":("fixed_development_candidates_observed_no_freeze" if observe else
                                    "frozen_choices_not_reselected" if report["review_scope"]=="new_post_freeze_batch" else
                                    "passed" if report.get("local_validation_passed") else "failed_no_freeze"),
                                "4_prospective_validation":("not_started_observe_only" if observe else
                                    "passed" if report.get("forward_validation_passed") else "not_passed"
                                    if report["review_scope"]=="new_post_freeze_batch" else "not_started"),
                                "5_acceptance":"protocol_implemented; prospective_evidence_pending"},
                  simulated_only=True)
    (root/"protocol.json").write_text(json.dumps(protocol,indent=2,sort_keys=True,allow_nan=False)+"\n")
    (root/"ledger.ndjson").write_text("".join(json.dumps(e,sort_keys=True,allow_nan=False)+"\n" for e in ledgers))
    batches=sorted({Path(path).parent.name for path in paths})
    overview["source_batches"]=batches
    (root/"next-round-report.json").write_text(json.dumps(overview,indent=2,sort_keys=True,allow_nan=False)+"\n")
    lines=["# SDDFV 运行与阶段审计", "", "输入批次："+", ".join(batches), "", "全部成交与盈亏均为模拟；历史数据已被查看，不是独立盈利验证。", "",
           f"运行/回放口径：{report['review_scope']}；观察模式不是冻结盈利验证。",
           f"本地门禁：{'通过' if overview['local_validation_passed'] else '未通过；禁止盈利验证升级'}。",
           "", "| 资产 | 分段 | 候选 | 成对样本 | 模型 Brier | 市场 Brier | 成交 | 扣费 PnL | 压力 PnL |",
           "|---|---|---|---:|---:|---:|---:|---:|---:|"]
    for group,details in report["groups"].items():
        for split,models in details["splits"].items():
            if split=="train": continue
            for name,value in models.items():
                x=value["execution"]
                lines.append(f"| {group} | {split} | {name} | {value['paired_model']['samples']} | {value['paired_model']['brier']} | {value['paired_market']['brier']} | {x['fills']} | {x['pnl']:.2f} | {value['cost_stress']['execution']['pnl']:.2f} |")
    lines += ["", "未选出候选不等于样本为零；逐候选失败原因见 report.json 的 candidate_checks。",
              "", "每秒参考价桶仅用于审计。尚未确认官方权重，不使用自制 spot 平均值替换官方 TWAP。",
              "虚拟初始本金 $1000、单笔 $5、总持仓成本上限 $20、单资产 $10；离线资金释放假设为到期后 120 秒。",
              "待结算持仓按成本列账，不是可成交盯市净值；周转收益率与账户收益率分开报告。",
              "", "freeze/start 必须通过本地门禁；observe 只用于补充参考价/费用/开盘价证据，不代表盈利验证。",
              "14 天新批次验收尚未发生，不能把工具开发完成当作前瞻验证通过。", ""]
    (root/"analysis.zh-CN.md").write_text("\n".join(lines))
    print(f"Next-round report: {root/'analysis.zh-CN.md'} | local_gate={overview['local_validation_passed']}")
    return overview


def freeze(directory):
    root=Path(directory)
    report=json.loads((root/"report.json").read_text())
    protocol=json.loads((root/"protocol.json").read_text())
    parity=json.loads((root/"parity-report.json").read_text())
    sha=digest(root/"model.json")
    if not report.get("local_validation_passed"):
        raise ValueError("local_validation_failed: no profitable validated candidate; use observe only for missing audit data")
    if report.get("model_sha256")!=sha or not parity.get("passed") or parity.get("forecasts",0)<=0 or parity.get("model_sha256")!=sha or protocol["model_sha256"]!=sha:
        raise ValueError("missing_or_mismatched_parity_protocol")
    value=dict(schema_version=1,model_sha256=sha,protocol_sha256=digest(root/"protocol.json"),
               local_validation_passed=True,online_trading_enabled=False)
    target=root/"freeze.json"
    if target.exists() and json.loads(target.read_text())!=value:
        raise ValueError("different_freeze_already_exists")
    target.write_text(json.dumps(value,indent=2,sort_keys=True)+"\n")
    print(f"Research protocol frozen: {target}; no real orders enabled")


def preflight(directory,journal,mode):
    root=Path(directory)
    sha=digest(root/"model.json")
    model=json.loads((root/"model.json").read_text())
    protocol=json.loads((root/"protocol.json").read_text())
    parity=json.loads((root/"parity-report.json").read_text())
    if (model.get("schema_version")!=2 or model.get("online_trading_enabled") is not False
            or not parity.get("passed") or parity.get("forecasts",0)<=0 or parity.get("model_sha256")!=sha
            or protocol.get("model_sha256")!=sha or protocol.get("assets")!=["btc","eth"]
            or protocol.get("execution_policy")!=model.get("policy") or protocol.get("max_duration_days")!=14):
        raise ValueError("incompatible_or_unverified_009_model_protocol")
    if mode=="start":
        if not (root/"freeze.json").exists():
            raise ValueError("start_blocked: no passed local freeze; run freeze first, or observe for audit data only")
        freeze=json.loads((root/"freeze.json").read_text())
        if not freeze.get("local_validation_passed") or freeze.get("model_sha256")!=sha or freeze.get("protocol_sha256")!=digest(root/"protocol.json"):
            raise ValueError("local_gate_or_freeze_mismatch")
    path=Path(journal)
    p=model["policy"]
    if path.exists():
        with path.open() as stream:
            for line in stream:
                try: event=json.loads(line)
                except ValueError: continue # Native journal repair owns an incomplete tail.
                if event.get("event")!="run_started": continue
                config=event.get("config",{})
                account=event.get("paper_account",{})
                if (event.get("audit_capture_version")!=capture_audit.CAPTURE_VERSION
                        or (event.get("terminal_model") or {}).get("sha256")!=sha
                        or config.get("symbols")!=["btc","eth"] or config.get("checkpoint_secs")!=30
                        or config.get("order_usdc")!=p["budget"]
                        or account.get("initial_balance_usdc")!=p["initial_balance"]
                        or account.get("max_total_exposure_usdc")!=p["max_exposure"]
                        or account.get("max_asset_exposure_usdc")!=p["max_asset_exposure"]):
                    raise ValueError("batch_contains_different_model_or_capture_revision: use NEW directory")
    path.parent.mkdir(parents=True,exist_ok=True)
    manifest_path=path.parent/"run-plan.json"
    now=int(dt.datetime.now(dt.timezone.utc).timestamp()*1000)
    if manifest_path.exists():
        manifest=json.loads(manifest_path.read_text())
        if (manifest.get("audit_capture_version")!=capture_audit.CAPTURE_VERSION
                or manifest["model_sha256"]!=sha or manifest["protocol_sha256"]!=digest(root/"protocol.json") or manifest["mode"]!=mode):
            raise ValueError("run_plan_changed: use NEW batch")
    else:
        manifest=dict(model_sha256=sha,protocol_sha256=digest(root/"protocol.json"),mode=mode,audit_capture_version=capture_audit.CAPTURE_VERSION,
                      started_at_ms=now,deadline_ms=now+protocol["max_duration_days"]*86400000)
        with manifest_path.open("x") as stream:
            stream.write(json.dumps(manifest,indent=2,sort_keys=True)+"\n")
    seconds=(manifest["deadline_ms"]-now)//1000
    if seconds<=0: raise ValueError("frozen_deadline_reached; do not extend until profitable")
    # Numeric output only, consumed by the shell; never print .env or keys.
    for value in (sha,seconds,p["initial_balance"],p["max_exposure"],p["max_asset_exposure"],p["budget"],
                  p["slippage_bps"],p["haircut"],p["failure_rate"],p["min_edge"],p["min_price"],p["max_price"]):
        print(value)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action",choices=("study","audit","freeze","preflight"))
    parser.add_argument("--input",nargs="+")
    parser.add_argument("--output",required=True)
    parser.add_argument("--journal")
    parser.add_argument("--mode",choices=("observe","start"),default="start")
    parser.add_argument("--initial-balance",type=float)
    parser.add_argument("--max-exposure",type=float)
    parser.add_argument("--max-asset-exposure",type=float)
    args=parser.parse_args()
    try:
        if args.action=="preflight":
            if not args.journal: parser.error("--journal required")
            preflight(args.output,args.journal,args.mode)
            return
        if args.action=="freeze":
            freeze(args.output)
            return
        if not args.input: parser.error("--input required for study/audit")
        if args.action=="study":
            budget_args=[]
            for key in ("initial_balance","max_exposure","max_asset_exposure"):
                if getattr(args,key) is not None:
                    budget_args.extend(["--"+key.replace("_","-"),str(getattr(args,key))])
            subprocess.run([sys.executable,str(Path(__file__).with_name("digital_fair_value_terminal_study.py")),
                            "--next-round","--symbols","btc,eth","--input",*args.input,"--output",args.output,*budget_args],check=True)
        audit(args.output,args.input)
    except (ValueError,FileNotFoundError,KeyError,TypeError) as error:
        parser.exit(2,f"Stage blocked: {error}\n")


if __name__=="__main__":
    main()
