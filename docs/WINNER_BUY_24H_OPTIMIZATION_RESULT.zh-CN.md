# Winner Buy 近 24 小时分析后的优化实施结果

## 1. 实施目标

本轮修改针对 `bot.log` 与三份 paper NDJSON 暴露出的关键问题：关闭市场无法回填、订单簿时间戳语义混淆、首要拒绝原因掩盖其他失败条件、重复市场污染统计，以及 paper 成交过于理想化。

本轮不放宽 Winner Buy 的边界、点差、深度或净 EV 参数，也不启用真实交易。

## 2. 已完成修改

### 2.1 关闭市场结算与历史回填

- 新增专用 `fetch_closed_markets_by_slug`，通过 SDK Gamma `MarketsRequest.closed(true)` 查询终局市场。
- Winner Buy 持仓结算和反事实评估回填均改用关闭市场查询。
- 启动后，evaluation journal 中已经到期且未解析的旧记录会自动再次查询，因此现有 29 条评估可自动补写 resolution。
- 新增空结果和非终局结果计数，避免空数组被静默当作成功。
- 新增 SDK transport 测试，确认结算请求明确携带 `closed=true`。

新增指标：

- `winner_buy_settlement_empty_results`
- `winner_buy_settlement_not_terminal`
- 对应 Prometheus `_total` 指标

### 2.2 订单簿 freshness 语义修复

`WinnerBookTop` 现在分别保存：

- `received_at_ms`：本进程收到 WS 快照或完成 REST 请求的时间；用于 freshness 判断。
- `exchange_updated_at_ms`：交易所报告的订单簿更新时间；仅用于诊断。

因此，刚刚拉取的 REST 快照不会因为盘口数秒没有变化而被误判为陈旧。

新增指标：

- `winner_buy_stale_ws_books`
- `winner_buy_stale_rest_books`
- `winner_buy_rest_fallbacks`

原有 `winner_buy_stale_books` 保留为总数，兼容既有监控。

### 2.3 完整过滤原因和 shadow EV

`ModelEvaluation` 新增 `failed_checks`：

- 保留 `rejection` 作为首要拒绝原因，确保交易行为和旧指标兼容。
- 对盘口数据有效的候选，同时计算点差、深度、反转边界和净 EV。
- evaluation NDJSON 新增 `failed_checks` 数组。
- 新增 `winner_buy_shadow_ev_failures`，即使候选先被边界或深度拒绝，也能看到它是否同时无法通过 EV。
- 任何包含 `ReversalBoundary` 的失败都会清除连续确认状态，不再只检查首要拒绝原因。

### 2.4 市场级去重结果

evaluation journal 在解析一个 slug 时，除逐 evaluation 写入 `resolved` 外，还会写入一条：

```json
{"action":"market_resolved", "slug":"...", "evaluation_count":2, "selected_evaluation_id":"..."}
```

该记录使用市场最后一次评估作为唯一决策样本，后续收益报告可直接按 `market_resolved` 统计，避免一个市场被重复扫描多次而放大权重。

### 2.5 更真实的 Winner Buy paper 延迟

新增配置：

```dotenv
PAPER_WINNER_BUY_DELAY_MS=250
```

Dry-run Winner Buy 在通过模型和风险检查后：

1. 先保留虚拟现金；
2. 等待配置的模拟延迟；
3. 重新读取真实 CLOB 多档盘口；
4. 按新的价格、深度、订单类型和流动性折扣模拟成交。

该延迟只作用于 dry-run，不会人为延迟未来的实盘下单。

### 2.6 分资产漏斗

运行指标新增 BTC、ETH 和其他资产的独立漏斗：

- scans
- signals
- no_book
- no_signal
- evaluations
- attempts
- fills
- resolved

Prometheus 示例：

```text
polypulse_winner_buy_asset_scans_total{asset="btc"} 123
polypulse_winner_buy_asset_evaluations_total{asset="eth"} 8
```

这可以判断之前 28 条 ETH、1 条 BTC 的不均衡究竟发生在哪个过滤阶段。

### 2.7 最大订单额度包含手续费

份额计算现在使用 `ask + fee_per_share` 作为有效入场价，因此 `WINNER_BUY_MAX_ORDER_USDC` 表示包含手续费的现金上限，不会再出现名义本金等于上限、加上手续费后实际计划成本略微超限的情况。

## 3. 配置变更

`.env` 和 `.env.example` 已加入：

```dotenv
PAPER_WINNER_BUY_DELAY_MS=250
```

其余 Winner Buy 阈值保持不变。当前仍应使用：

```dotenv
DRY_RUN=true
```

## 4. 验证方法

第一次启动后重点确认：

1. 旧 evaluation journal 出现 `resolved` 和 `market_resolved`。
2. `winner_buy_evaluations_resolved` 大于 0。
3. `winner_buy_settlement_empty_results` 不持续增长。
4. `winner_buy_stale_rest_books` 应显著下降，正常情况下接近 0。
5. 分资产指标能解释 BTC/ETH 在 scan、no-book、no-signal 和 evaluation 阶段的差异。
6. 新 evaluation 同时包含 `rejection` 和 `failed_checks`。

建议先运行 72 小时验证数据闭环，再累计 7–14 天及至少 100 个独立 `market_resolved` 样本。参数调整必须以市场级、含手续费并考虑模拟延迟后的结果为依据。

## 5. 尚未执行的策略参数调整

以下事项有意延后，而不是遗漏：

- 不降低 `WINNER_BUY_MIN_DEPTH_USDC`。
- 不放宽 `WINNER_BUY_REVERSAL_SIGMA`。
- 不降低资产模型风险缓冲。
- 不扩大真实资金订单额度。
- 暂不使用可见深度动态扩大仓位。

原因是此前的自动结果回填链路失效，现阶段没有足够、无重复且执行条件真实的样本支持这些调整。
