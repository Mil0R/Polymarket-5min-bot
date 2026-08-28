# Bayesian Coherence Shadow Scanner

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

## 运行

```bash
# 单次发现和盘口快照
./target/release/bayesian_arb scan

# 持续 Shadow
./target/release/bayesian_arb shadow

# 汇总结果
./target/release/bayesian_arb report results/bayesian-arbitrage/shadow-run-001/events.ndjson
```

优化后的第二轮默认使用：

```bash
./target/release/bayesian_arb report results/bayesian-arbitrage/shadow-run-002/events.ndjson
```

报告会自动合并同一路径的 `.1`、`.2` 等轮转分片。

持续模式仅在净边际、规模、通过状态或拒绝原因发生变化时写日志；无变化时按 `BAYESIAN_UNCHANGED_LOG_SECONDS` 写一次心跳快照。

持续模式还会：

- 按 `BAYESIAN_RELATIONSHIP_REFRESH_SECONDS` 重新发现关系，并记录新增/移除关系；
- 为每个关系保存完整的 `1/2/5/10/20/50/100` 股规模曲线，而不只保存最终选中档位；
- 用 `discovery_summary` 解释 NegRisk 候选因不可交易、缺少 Other 或腿数超限而被排除的数量；
- 按 `BAYESIAN_SUMMARY_SECONDS` 记录扫描摘要；
- 按 `BAYESIAN_RESULTS_MAX_BYTES` 轮转结果，并保留 `BAYESIAN_RESULTS_RETENTION_FILES` 个分片。
- `BAYESIAN_SEMANTIC_ALLOWLIST` 可标记已经人工核对的 relationship ID，但只改变研究标签，不会令关系获得实盘资格。

## 结果事件

- `relationship_discovered`：关系、收益下界、语义证据及所有交易腿；
- `relationship_evaluation`：盘口深度、VWAP、手续费、缓冲和净收益；
- `discovery_summary`：发现覆盖和 NegRisk 淘汰原因；
- `relationship_added` / `relationship_removed`：刷新后的关系变化；
- `cycle_summary`：周期数、完整盘口数、最佳边际及累计机会数；
- `shadow_opportunity`：达到净边际及最低组合金额的快照；
- `run_started` / `run_stopped`：运行配置和计数。

`shadow_opportunity` 只是多个独立订单簿在相近时刻的可成交快照，不代表多腿可以原子成交，也不代表已实现收益。
