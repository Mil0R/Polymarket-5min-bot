# Cross-Market Probability Arbitrage Shadow Scanner

该模块用于研究 Polymarket 跨市场逻辑一致性，不包含签名器和下单代码，不能进行实盘交易。

## 当前覆盖关系

### 截止日期包含关系

同一个 Gamma Event 中按截止日期排列的累计事件，例如：

```text
事件在 6 月 30 日前发生 ⊆ 事件在 12 月 31 日前发生
```

Shadow 组合为：

```text
YES(较晚截止日) + NO(较早截止日)
```

如果两条规则确实描述同一事件，组合在所有状态下至少支付 1 USDC/share。
自动发现的日期关系标记为 `template_inferred_requires_rule_review`，不能据此直接实盘。

### NegRisk 完备集合

仅当 Gamma 标记为 NegRisk、存在 `negRiskOther`，且所有已部署子市场当前均可交易时，检查：

- 全部 YES：最低支付 1；
- 全部 NO：N 个结果时最低支付 N-1。

腿数超过 `BAYESIAN_MAX_PARTITION_LEGS` 的集合会跳过。

## 成本模型

每个规模档位都会逐档消耗所有腿的实时 ask 深度，并按照每个盘口价格和该市场的 fee schedule 计算 taker fee：

```text
net_profit = guaranteed_payout - raw_cost - taker_fees - execution_buffer
net_edge   = net_profit / guaranteed_payout
```

任一腿缺失盘口或深度不足，整个组合 fail-closed。

主判定仍使用 `BAYESIAN_EXECUTION_BUFFER_PCT=0.003`。同一个规模档位还会按
`BAYESIAN_EXECUTION_BUFFER_CURVE_PCT=0,0.003,0.005,0.01` 同时计算多条缓冲曲线，
用于判断表面价差对延迟、滑点和多腿非原子执行是否敏感。这些曲线不会改变主判定阈值。

## 运行

```bash
# 单次发现和盘口快照
./target/release/cross_market_probability_arbitrage scan

# 持续 Shadow
./target/release/cross_market_probability_arbitrage shadow

# 汇总结果
./target/release/cross_market_probability_arbitrage report results/cross-market-probability-arbitrage/shadow-run-003/events.ndjson
```

报告会自动合并同一路径的 `.1`、`.2` 等轮转分片。第三轮默认输出：

- `events.ndjson`：关系明细、近阈值进出、机会快照；
- `summary.ndjson`：运行生命周期、发现摘要、关系刷新和周期摘要。

## 变化触发和分级日志

持续模式优先使用 WebSocket 盘口缓存，在每个 `BAYESIAN_POLL_SECONDS` 周期只重新评估
盘口签名发生变化的关系；为避免丢失事件，每 `BAYESIAN_FULL_SCAN_SECONDS` 执行一次全量评估。
WebSocket 数据超过 `BAYESIAN_WEBSOCKET_BOOK_MAX_AGE_MS` 后自动回退到 HTTP 快照。

- 净边际达到 `BAYESIAN_NEAR_MISS_EDGE` 但未达成交阈值时，记录 `near_miss_entered`；
- 近阈值关系按 `BAYESIAN_NEAR_MISS_LOG_SECONDS` 保留明细心跳；
- 普通负边际关系按 `BAYESIAN_ORDINARY_LOG_SECONDS` 保留明细心跳；
- `near_miss` 只是研究标签，不会放宽 `BAYESIAN_MIN_NET_EDGE` 或触发下单。


## 语义审核文件

`BAYESIAN_SEMANTIC_REVIEW_FILE` 指向一个以 `relationship_id` 为键的 JSON 对象：

```json
{
  "deadline:event-id:early-condition:late-condition": {
    "status": "rules_equivalent",
    "reviewed_at": "2026-08-28T12:00:00Z",
    "notes": ["Same resolution source and event definition; only deadline differs."]
  }
}
```

允许的 `status` 是 `rules_equivalent`、`manually_approved` 和 `rejected_semantic_mismatch`。
未知状态会标记为 `review_status_invalid_fail_closed`；文件缺失时继续使用模板推断标签。
无论审核状态如何，本二进制均强制 `live_eligible=false`。

持续模式还会：

- 按 `BAYESIAN_RELATIONSHIP_REFRESH_SECONDS` 重新发现关系，并记录新增/移除关系；
- 为每个关系保存完整的 `1/2/5/10/20/50/100` 股规模曲线，而不只保存最终选中档位；
- 为每个完整规模档位保存多执行缓冲情景；
- 用 `discovery_summary` 解释 NegRisk 候选因不可交易、缺少 Other 或腿数超限而被排除的数量；
- 按 `BAYESIAN_SUMMARY_SECONDS` 记录扫描摘要；
- 按 `BAYESIAN_RESULTS_MAX_BYTES` 轮转结果，并保留 `BAYESIAN_RESULTS_RETENTION_FILES` 个分片。
- `BAYESIAN_SEMANTIC_ALLOWLIST` 可标记已经人工核对的 relationship ID，但只改变研究标签，不会令关系获得实盘资格。

## 结果事件

- `relationship_discovered`：关系、收益下界、语义证据及所有交易腿；
- `relationship_evaluation`：盘口深度、VWAP、手续费、缓冲和净收益；
- `near_miss_entered` / `near_miss_exited`：进入或离开近阈值观察集；
- `discovery_summary`：发现覆盖和 NegRisk 淘汰原因；
- `relationship_added` / `relationship_removed`：刷新后的关系变化；
- `cycle_summary`：周期数、完整盘口数、最佳边际及累计机会数；
- `shadow_opportunity`：达到净边际及最低组合金额的快照；
- `run_started` / `run_stopped`：运行配置和计数。

`shadow_opportunity` 只是多个独立订单簿在相近时刻的可成交快照，不代表多腿可以原子成交，也不代表已实现收益。
报告会按关系类型、语义状态、规模档位和执行缓冲分组，方便区分逻辑候选覆盖与真正的可执行空间。
