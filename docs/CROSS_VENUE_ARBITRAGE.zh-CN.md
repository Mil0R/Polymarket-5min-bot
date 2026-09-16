# Cross-Venue Arbitrage 验证模块

独立可执行文件 `cross_venue_arbitrage`，用于验证 PMP 候选能否在 Kalshi 与 Polymarket 国际盘口构成经济性合理的等量对冲。当前为只读研究：不加载交易私钥、不发送订单、不记录假成交。没有自动进入实盘的开关。

## 运行

```bash
cargo build --release --bin cross_venue_arbitrage
./target/release/cross_venue_arbitrage scan
./target/release/cross_venue_arbitrage leads
./target/release/cross_venue_arbitrage shadow
./target/release/cross_venue_arbitrage report
```

服务器需要 `.env` 和二进制。默认配对登记文件位于 `results/cross-venue-arbitrage/pairs.json`；文件不存在时程序会自动创建空数组，允许先采集 PMP 候选。也可以配置登记文件的绝对路径。在包含 `.env` 的工作目录执行：

```bash
./cross_venue_arbitrage shadow
```

`scan` 执行一轮候选获取及已审核配对的盘口验证。`shadow` 持续运行，Ctrl+C 后生成报告；异常退出后，下一次继续追加，历史记录不覆盖。只有 ARB / ARB_FLAGGED / WATCH 标签通过后，才检查数据年龄和登记配对；全部 ALIGNED 时，即使配对齐全也不会自动探测盘口。`report` 离线读取最新 run_id，使用该运行记录的参数。每日分段 NDJSON 便于增量同步，不自动删除历史分段。同一目录只允许一个写进程。

`leads` 从最新运行报告生成 `pair-leads.json` 人工审核队列，包含 sport/game、观察次数、独立来源快照、最大显示价差、标签和字段一致性诊断。PMP 行缺少精确合约 ID、比赛日期及明确 outcome，因此该命令不会改写 `pairs.json`，也不会把模糊标题自动批准为交易配对。审核者必须独立定位两边合约后再执行 `inspect`。

默认保存位置：

- `logs/cross-venue-arbitrage/shadow-run-002/system.log`：控制台摘要对应的独立系统日志。
- `results/cross-venue-arbitrage/shadow-run-002/events-YYYY-MM-DD.ndjson`：原始 MCP 响应、候选、拒绝原因、规则元数据、盘口、报价和延迟观测。
- 同目录 `report.json`：每次生成的最新报告；原始事件一直追加。
- 同目录 `pair-leads.json`：运行 `leads` 后生成的未审核发现清单；不是交易登记表。

## Pro 与免费接口

自行在 `.env` 填写 `PMP_API_KEY`。代码只将 Key 发送到固定 PMP MCP 地址，禁止重定向，不记录 Key。`pro_key_present` / `api_key_present` 只说明本地有 Key；当前不自动验证服务端订阅权益，因此 `pro_entitlement_verified=false`、`coverage_status=unverified`。`returned=total_count` 说明接口声明的条数匹配，不证明覆盖全市场。`truncated` 缺失时明确记录 unknown。

PMP 每 120 秒读取一次；按 `as_of` 自行计算年龄，默认大于 900 秒或供应商 `stale=true` 时拒绝。现有样本来源时间戳约 15 分钟更新一次；900 秒只用于候选发现，不表示报价仍可成交。盘口的 1 秒 REST / 5 秒年龄限制独立生效。报告分别统计年龄超限、vendor stale 和合并 stale；这些扫描计数不等于被拒绝的套利机会。`ARB` / `ARB FLAGGED` 为主候选，`WATCH` 默认同时采集对照。

报告同时审计 `cheaper_venue` 与两个显示 implied percentage 的方向。`displayed_direction_mismatch` 只表示在“两个百分比代表同一结果”的解释下出现表面矛盾；由于来源缺少明确 outcome，该指标不能单独证明供应商数据错误。任何候选都必须回到精确合约和实际盘口验证。

供应商明确返回 `No arbitrage data available right now.` 时，保存原始响应并记录 `provider_no_data`，等待下一轮；不能把该响应解释为已确认没有套利或网络故障。HTTP 5xx、网络错误和明确临时 RPC 错误最多尝试 3 次（2s / 4s 两次退避），每次记录 `feed_attempt`。401/403、429、schema 和未知工具错误不进行短间隔重试。历史日志原始错误用语保留在 `rejections`；`feed_health` 按原始响应重分类，统计不可重复相加。

## 配对登记与规则审核

默认登记文件为 `results/cross-venue-arbitrage/pairs.json`，初始内容为 `[]`，不会自动把标题相似的合约认定为同一结算事件。第一阶段即使没有登记配对，也可以运行，收集供应商字段、时间、候选数量以及缺失映射原因。

登记文件是 JSON 数组，每条结构如下（全部示例值必须替换）：

```json
[
  {
    "id": "unique-event-pair-id",
    "sport": "NFL",
    "game": "EXACT PMP GAME LABEL",
    "kalshi_ticker": "EXACT-KALSHI-MARKET-TICKER",
    "poly_slug": "exact-polymarket-market-slug",
    "poly_yes_token": "111",
    "poly_no_token": "222",
    "reviewed_digest": null,
    "review_notes": "Explain exact same event/date/outcome; overtime, draw, cancellation, postponement and source treatment",
    "valid_until": "2026-09-15T00:00:00Z",
    "poly_fee_cap_per_net_share": null,
    "kalshi_fee_cap_per_contract": null,
    "fee_evidence": "Fee sources, checked time, rounding; equal NET shares after any in-share fee"
  }
]
```

`poly_yes_token` 必须代表与 Kalshi YES 完全相同的结果，`poly_no_token` 代表其互补结果。体育市场的显示结果可能是球队名称，而非 Yes/No；必须由审核者核对 token 与对应结果，程序只验证两个 token 均属于指定 Gamma 市场且互不相同。PMP 仅提供 game 标签时，标签匹配只用于触发精确登记的配对，不用于自动推断结算等价。不要为重复比赛复用旧审核。

先填完配对、规则说明、审核有效期和费用上界，再运行：

```bash
./cross_venue_arbitrage inspect
```

控制台和 `review_proposal` 记录两个平台的规则与 `proposed_reviewed_digest`。审核者逐条核对事件日期、胜方身份、加时/平局、取消/延期、裁决和数据源后，将指纹复制到登记文件的 `reviewed_digest`。登记文件每个周期重新加载，不用重启；`.env` 参数改变需要重启。

`inspect` 只读取规则和元数据，不验证盘口。审核完成后执行独立盘口诊断：

```bash
./cross_venue_arbitrage probe
```

`probe` 不调用 PMP，最多检查登记文件前 `MAX_PAIRS` 个配对（默认 5 个）。同样要求审核指纹、有效期、费用上限、可交易状态与盘口检查全部通过；满足净收益门槛时保存延迟复核。空登记文件会给出明确错误和诊断报告；超过数量上限的配对会计为 deferred，可调整上限或使用专门的诊断登记文件。

结果分别保存到 `${CROSS_VENUE_ARBITRAGE_RUN_DIR}/inspect/`、`${CROSS_VENUE_ARBITRAGE_RUN_DIR}/probe/`，各自自动生成 `report.json`；系统日志位于配置日志的同级 `system-inspect.log` / `system-probe.log`。因此可以在 Shadow 运行期间诊断，诊断运行不会替换主目录报告的最新 Shadow 批次。诊断样本只能验证人工配对的盘口链路，不是 PMP 自动发现机会的证据。

规则、token、配对配置或审核内容变化会改变指纹，之前的批准立即失效。有效期过期也会阻止收益估算。仅保存指纹不代表规则自动证明正确，审核者需要承担其语义检查。

费用未知时填写 null，禁止自动按零费计算。费用采用人工审核的每份费用上界，Kalshi 按订单向上取整到美分。Polymarket 如果以份额扣费，上界必须覆盖为取得一份**净兑付份额**而增加的购买及费用成本。Gamma `feeSchedule` 也参与审核指纹；Kalshi 的费用证据须独立核对并设置短有效期。此版本不声称自动取得了账户实际费率。

## 盘口与收益实验

- 每条已审核候选并行请求 Kalshi 盘口及两个 Polymarket token 盘口。
- Kalshi NO ask = 1 − YES bid；YES ask = 1 − NO bid，保留对档数量。
- 按 5 / 10 / 25 份走盘口，默认 50% 深度折扣、20 bps 额外滑点，每个平台最多 500 USD 虚拟资金。
- 两种方向均计算：Poly YES + Kalshi NO、Poly NO + Kalshi YES。
- 只对净收益达到每对 0.02 USD 的候选，在 0.5 / 1 / 2 / 5 秒复核同一方向、同一数量。
- 保存两腿重新报价，以及先锁住 Poly / 先锁住 Kalshi 的顺序成交压力场景；请求失败和深度不足均保留。
- 每个 run 内，每个配对仅选第一次出现的最高净收益方向/数量进入汇总，重复扫描不累加利润；不同资产/比赛仍可能相关。

Polymarket 检查 token 身份、盘口 timestamp（默认最长 5 秒）和请求耗时（默认最长 1 秒），未来时间最多容忍 2 秒。Kalshi 标准订单簿没有在此版本可验证的交易所更新时间，故仅验证请求/接收时间；所有正值都称为 `conditional`，不能计为真实成交或作为实盘晋级证据。

`conditional_worst_delay_profit_sum` 是首个机会在所有延迟与顺序场景中的最差正收益之和，不是账户 PnL。未通过场景、部分成交损失、订阅费、资金占用与再平衡费用不可忽略。`actual_fills=0`、`realized_pnl=0`、`live_ready=false` 固定输出。

报告分别输出整个目录和所选运行的坏行数量。旧运行的截断记录保留用于审计，但不会阻止完整的新运行被标记为稳健；属于当前运行或无法归属的坏行仍会 fail closed。建议每轮模型更新使用独立批次目录，避免同步中的半行和历史数据混淆。

## 分阶段验收

1. 核验服务端 Pro 权限和数据覆盖，并收集来源年龄、标签分布与映射计数。已有足够来源刷新样本时，无需强制继续空配对运行；单靠延长轮询无法证明订阅权限。
2. 审核少量短期、明确结算规则的配对，先用 `probe` 验证盘口链路，再继续 Shadow。7～14 天可作为观察预算；是否充分取决于独立配对、有效盘口和延迟复核覆盖。
3. 有足够独立、规则一致且延迟后仍为正的配对，才开发更完整的 Kalshi 行情/费用校准、订单失败和资金占用模型。缺少这些证据时不开发自动交易。

## 本地验证记录（2026-09-14）

- `cargo test cross_venue_arbitrage --lib`：13 项通过，覆盖盘口取反、费用/预算、未知数据、审核失效、MCP SSE、失败请求、feed 错误分类与重试、诊断运行隔离、跨 run 报告隔离、重复机会和未完成延迟。
- `cargo build --release --bin cross_venue_arbitrage`：成功。
- 实际公开 MCP `scan`：返回 1 条 ALIGNED，`truncated=true`，首次信号年龄约 1 秒；限时 Shadow 的下一轮为约 20 秒。
- 限时 Shadow 自动停止并追加新 run，离线 report 正确选择最新 run，没有重复统计之前的扫描。
- 冒烟结果单独保存在 `results/cross-venue-arbitrage/local-smoke-001/`；默认服务器批次 `shadow-run-001` 未使用。
- 未使用 Pro Key，尚未验证 Pro 完整字段。空登记文件下没有真实已审核配对，跨平台实际盘口链路尚待配对验收；当前经济性逻辑由确定性测试覆盖。

参考：[PMP MCP](https://predictionmarketspicks.com/mcp)、[PMP 扫描器](https://predictionmarketspicks.com/tools/arb-scanner)、[PMP 使用条款](https://predictionmarketspicks.com/tos)、[Kalshi 盘口](https://docs.kalshi.com/getting_started/orderbook_responses)、[Polymarket 盘口](https://docs.polymarket.com/api-reference/market-data/get-order-book)。
