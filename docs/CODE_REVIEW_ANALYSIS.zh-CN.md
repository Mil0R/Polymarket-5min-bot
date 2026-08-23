# Polymarket 5-Minute Bot 代码审查、运行日志与修复建议报告

> 更新时间：2026-08-20  
> 本文合并两部分结论：现有代码静态审查，以及 `bot-08201550.log` 最近 6 小时 Winner Buy dry-run 运行分析。  
> 本文同时记录实施状态；截至本次更新，P0～P3 已完成。代码级整改已闭环，策略收益有效性仍需新的 dry-run 样本验证。

## 目录

- [一、审查范围与总体结论](#一审查范围与总体结论)
- [二、Winner Buy 六小时运行分析](#二winner-buy-六小时运行分析)
- [三、严重级别问题](#三严重级别问题)
- [四、高危级别问题](#四高危级别问题)
- [五、中危级别问题](#五中危级别问题)
- [六、统一修复建议对照表](#六统一修复建议对照表)
- [七、建议实施顺序与验收标准](#七建议实施顺序与验收标准)
- [八、实施进度记录](#八实施进度记录)

---

## 一、审查范围与总体结论

本次合并审查覆盖以下模块：

- 市场发现、5 分钟窗口调度与 WebSocket 生命周期；
- YES + NO 双腿套利检测、报价和顺序执行；
- PositionTracker、PositionBalancer、HedgeMonitor 与 WindDown；
- Winner Buy 尾盘单腿策略的信号、模拟成交、持仓、结算和收益统计；
- dry-run 虚拟资金账本、运行指标和 TUI 可观测性。

总体结论如下：

1. 原代码审查中的 6 项问题仍应纳入修复计划，其中 MarketScheduler 时序、WindDown/Winner Buy 冲突属于严重问题。
2. 六小时日志又暴露出 Winner Buy 的两个更优先问题：**尚未结束的实时赔率被当作最终结算结果**，以及**待结算仓位不能真正跨窗口保存**。
3. 当前 Winner Buy 仅根据市场盘口本身选择 0.95～0.98 的一边，没有独立概率优势。`1 - 买入价` 是“胜出时毛利润”，不是确定性套利利润。
4. 日志中的 `winner_buy_wins=6`、`winner_buy_losses=2` 和 `realized=-$37.1585` 均不是可信的真实结算结果，不能据此调整阈值或决定进入实盘。
5. 在 P0、P1 问题完成修复并重新积累足够 dry-run 样本前，不应开启极小额真实资金测试。

---

## 二、Winner Buy 六小时运行分析

### 2.1 分析范围

- 日志文件：`bot-08201550.log`
- 文件覆盖：北京时间约 2026-08-20 09:11～15:47
- 严格的最近 6 小时窗口：北京时间约 09:47～15:47
- 09:45 时 Winner Buy 累计尝试仍为 0，因此下面 9 次尝试全部位于目标时间段内。
- 运行模式：`DRY_RUN=true`
- 策略参数：最后 60 秒启用，买价 0.95～0.98，单次名义金额上限 20 USDC，每窗口最多 1 次。

### 2.2 逐笔记录

| 北京时间 | 市场 | 方向 | 限价 | 请求份额 | 成交份额 | 下单时剩余 | 日志判定 |
| :--- | :--- | :---: | ---: | ---: | ---: | ---: | :--- |
| 10:09 | ETH | NO | 0.98 | 20.40 | 20.40 | 57 秒 | 盈利 |
| 10:59 | BTC | YES | 0.98 | 20.40 | 20.40 | 48 秒 | 盈利 |
| 11:24 | ETH | YES | 0.98 | 20.40 | 20.40 | 55 秒 | 盈利 |
| 12:59 | BTC | NO | 0.96 | 20.83 | 20.83 | 57 秒 | 盈利 |
| 13:33 | ETH | NO | 0.98 | 20.40 | 20.40 | 60 秒 | 亏损 |
| 13:39 | ETH | NO | 0.98 | 20.40 | 20.40 | 41 秒 | 亏损 |
| 15:08 | ETH | YES | 0.97 | 20.61 | 20.61 | 60 秒 | 盈利 |
| 15:39 | BTC | NO | 0.96 | 20.83 | 0 | 24 秒 | 未成交 |
| 15:44 | ETH | YES | 0.96 | 20.83 | 3.75 | 46 秒 | 盈利 |

累计表面指标：

- 下单尝试 9 次，有成交 8 次，表面成交率 88.9%；
- 7 次完全成交，1 次部分成交，1 次未成交；
- 模拟买入成本合计约 143.55 USDC；
- 假设全部最终胜出的条件毛利润为 3.6415 USDC；
- 日志显示 6 胜 2 负，所谓“已实现盈亏”为 -37.1585 USDC。

### 2.3 为什么上述胜率和盈亏无效

8 笔成交全部在对应市场结束前约 20～56 秒就被记录为 `settled (realized)`。例如：

- 第一笔在剩余 57 秒时买入，却在市场结束前约 45 秒被判定结算；
- 第五笔在剩余 60 秒时买入，仅约 5 秒后就被判定亏损；
- 最后一笔在市场结束前约 31 秒被判定盈利。

`settle_winner_buy()` 只检查 YES/NO 的一边是否大于等于 0.5、另一边是否小于 0.5。对绝大多数仍在交易的二元市场，这个条件已经成立。随后代码直接按终局 1/0 赔付计算盈亏，造成活动市场被提前结算。

因此：

- 6 胜 2 负只是结算轮询时的盘口方向，不是最终市场结果；
- -37.1585 USDC 是伪“已实现”盈亏；
- 两次表面亏损虽然不能证明最终真的失败，但说明 0.98 盘口在买入后 5～12 秒内就可能翻转，单次快照并不稳定；
- 日志没有保存真正闭市后的最终 outcome，无法从该文件恢复真实胜率。

### 2.4 Winner Buy 的收益不对称

以 20 USDC 名义金额为例：

| 买价 | 胜出毛利润 | 失败本金损失 | 一次失败约抵消的成功次数 | 忽略费用的盈亏平衡胜率 |
| ---: | ---: | ---: | ---: | ---: |
| 0.95 | 约 1.05 USDC | 约 20 USDC | 19 次 | 95% |
| 0.96 | 约 0.83 USDC | 约 20 USDC | 24 次 | 96% |
| 0.98 | 约 0.41 USDC | 约 20 USDC | 49 次 | 98% |

本次成交加权平均价格约为 0.975，忽略费用时真实胜率也必须高于约 97.5% 才能盈亏平衡。加入 taker fee、滑点、盘口变化和模拟偏差后，要求更高。

仅观察“胜方价格很高”并不能建立优势，因为 0.98 本身已经表达了市场约 98% 的隐含概率。策略必须证明自己的独立胜率估计高于成交价格和费用之和，否则这只是高胜率、低赔率、带尾部巨亏的方向性交易。

### 2.5 其他运行现象

1. 整个运行期双腿套利没有成交，末尾指标显示 `opportunities_admitted=0`、`depth_rejections=1692`，本次运行主要验证到了 Winner Buy 路径。
2. 日志出现约 96 次 WebSocket `ConnectionClosed` 或握手异常，但 `stream_reconnects` 最终仍为 0，连接健康指标没有准确反映底层断线。
3. 最后一笔仅成交 3.75/20.83，说明 FAK/模拟盘口能够体现部分成交；但 Winner Buy 的收益统计使用信号限价而不是完整成交均价，后续应统一按真实 fill 明细记账。
4. `PAPER_INITIAL_BALANCE_USDC=100`，而 Winner Buy 累计模拟成本约 143.55 USDC。当前路径没有完整接入 paper reservation/commit 和待赎回资金占用，虚拟额度不能可靠约束该策略。

---

## 三、严重级别问题

### 1. MarketScheduler 窗口预刷新时序错误

- **涉及文件**：`src/market/scheduler.rs`
- **严重等级**：严重（Critical）
- **问题**：调度器提前唤醒后仍以当前时间向下对齐，可能再次请求只剩数秒的旧窗口。

典型路径：下一窗口为 12:05:00，调度器在 12:04:55 提前醒来，却调用 `calculate_current_window_timestamp(12:04:55)` 得到 12:00:00。旧市场仍活跃，接口返回成功，机器人重新订阅旧窗口并在几秒后再次 rollover。

**建议**：预刷新时明确查询下一窗口时间戳，或以 `now + refresh_advance_secs` 计算目标窗口；增加跨边界单元测试，确保不会重复进入旧窗口。

### 2. WindDown 与 Winner Buy 持有至结算目标冲突

- **涉及文件**：`src/wind_down_service.rs`、`src/main.rs`
- **严重等级**：严重（Critical）
- **问题**：两项配置同时开启时，WindDown 可能把 Winner Buy 高价买入、计划持有至结算的单腿仓位当作多余敞口，以极低兜底价卖出。

**建议**：

- 为持仓增加来源和生命周期标签，例如 `ArbitrageLeg`、`WinnerHoldToSettlement`、`Recovery`；
- WindDown 明确排除 `WinnerHoldToSettlement`；
- 在缺少仓位标签的版本中，对 Winner Buy 与 WindDown 做启动配置互斥检查；
- 不应只靠当前 `.env` 中 WindDown 为 0 来规避潜在代码冲突。

### 3. 活动市场赔率被误判为最终结算结果

- **涉及文件**：`src/winner_buy.rs:37-57`、`src/main.rs:807-861`
- **严重等级**：严重（Critical）
- **日志证据**：全部 8 笔成交均在市场结束前被记为 realized。
- **根本原因**：以 `>= 0.5` 判断赢家，没有检查市场是否 closed/resolved，也没有验证最终 outcome 是否稳定为 1/0。

**建议**：

1. 只有在 Gamma/链上状态明确 closed、resolved 或可赎回时才允许结算；
2. outcome prices 必须接近确定的 1/0，不能把 0.51/0.49 或 0.98/0.02 当作终局的充分条件；
3. 保存 condition id、token id、窗口结束时间和 resolution 状态；
4. 未决状态继续保留，不计入 win/loss/realized PnL；
5. 将 `estimated`、`mark-to-market`、`realized` 三种收益彻底分开。

### 4. Pending Winner Buy 不能真正跨窗口和跨重启保存

- **涉及文件**：`src/main.rs:425-428`
- **严重等级**：严重（Critical）
- **问题**：`pending_winners` 在每个外层窗口循环内部创建。若修正为等待真实终局，rollover 时待结算列表会被销毁。

**建议**：

- 将待结算仓位提升到进程级共享状态；
- 写入交易 journal，并在启动时恢复；
- 结算服务与当前盘口订阅解耦，即使对应 5 分钟窗口已经切换也继续轮询；
- 使用 `slug + token_id + fill_id` 做幂等键，防止重复结算；
- 增加“跨窗口后结算”“重启恢复后结算”“重复响应不重复记账”测试。

### 5. Winner Buy 把条件收益误认为可交易优势

- **涉及文件**：`src/winner_buy.rs`、`src/runtime_metrics.rs`
- **严重等级**：严重（Critical，策略与资金风险）
- **问题**：当前信号只复述市场盘口方向，没有独立估计真实胜率；代码和注释却把 1%～5% 的胜出条件收益描述为 edge。

**建议**：建立明确的期望值门槛：

```text
EV = p_model × (1 - effective_price)
     - (1 - p_model) × effective_price
     - fees
     - slippage_buffer
     - model_risk_buffer
```

只有 `EV > min_expected_value` 且 `p_model` 有独立来源时才允许入场。独立信息至少应包括结算价格源、开盘基准、当前/预计 TWAP 和剩余时间反转风险，不能只使用同一个 CLOB ask 同时充当概率估计和成交价格。

---

## 四、高危级别问题

### 6. PositionBalancer 在单边过剩时反向撤销对冲方订单

- **涉及文件**：`src/risk/position_balancer.rs`
- **严重等级**：高危（High）
- **问题**：当 YES 已多于 NO 时，除撤销 YES 买单外，还会撤销本应补齐缺口的 NO 买单，使失衡无法自然修复。

**建议**：过剩方停止增仓并撤销其买单，赤字方的对冲买单应保留；只有挂单本身会制造反向过量时，才按超额部分撤销。增加 YES 过剩、NO 过剩、已有挂单足够/不足四组表驱动测试。

### 7. PositionTracker API 同步造成名义敞口虚高

- **涉及文件**：`src/risk/positions.rs`
- **严重等级**：高危（High）
- **问题**：周期同步缺少真实成本时按每份 1 USDC 重建成本。成对的 100 YES + 100 NO 可能从实际约 95 USDC 被记成 200 USDC，误触发风险上限。

**建议**：区分“现金成本”“最坏损失”“名义份额”和“可合并配对价值”；已对冲份额按组合风险计算，只有不平衡单腿按保守最坏损失计入。API 同步不得无条件覆盖本地已知成交成本。

### 8. Winner Buy 在 60 秒 TWAP 仍形成时过早入场

- **涉及文件**：`src/main.rs:793-992`、`src/winner_buy.rs`
- **严重等级**：高危（High）
- **日志证据**：配置为最后 60 秒入场，而日志市场配置显示 `twapEnabled=true`、`twapLookbackSeconds=60`；两次 0.98 方向在买入后数秒即发生盘口翻转。

**建议**：

- 将更短入场窗口作为临时安全措施，而不是最终模型；
- 读取与最终判定一致的官方价格源和 TWAP 规则；
- 计算当前 TWAP、剩余样本对 TWAP 的最大影响、距离开盘阈值的安全边际；
- 要求方向连续保持若干次或若干秒，且 bid/ask、spread、可成交深度共同确认；
- BTC、ETH 分开统计和校准，不共享未经验证的统一阈值。

### 9. Winner Buy 绕过统一风险与 dry-run 资金账本

- **涉及文件**：`src/trading/executor.rs:300-318`、`src/main.rs:939-965`、`src/trading/paper.rs`
- **严重等级**：高危（High）
- **问题**：dry-run 只模拟盘口成交，没有调用 paper reserve/commit；Winner Buy 也没有完整进入风险预留、PositionTracker、journal 和待赎回资金生命周期。

**建议**：

- 单腿策略与套利策略共用下单前风险门；
- dry-run 下先预留名义成本，成交后按真实平均价扣减现金；
- 仓位真实结算或模拟赎回后才释放资金；
- 将部分成交、未成交、手续费和赎回延迟计入账本；
- 添加单策略上限、全局总敞口、连续失败熔断和每日亏损上限。

---

## 五、中危级别问题

### 10. HedgeMonitor 异步止盈止损存在竞态

- **涉及文件**：`src/risk/hedge_monitor.rs`
- **严重等级**：中危（Medium）
- **问题**：同一持仓在高频行情、断线恢复或对账并发下，可能重复触发平仓或丢失 `processing` 状态。

**建议**：使用基于 `pair_id` 的显式状态机和原子状态转换，例如 `Open → Closing → Closed/Failed`；下单请求使用幂等键，并对同一仓位加互斥保护。

### 11. 套利检测与执行存在双重舍入和 Tick 偏差

- **涉及文件**：`src/monitor/arbitrage.rs`、`src/trading/executor.rs`
- **严重等级**：中危（Medium）
- **问题**：检测阶段先保留两位小数，执行阶段再加滑点并向上对齐 tick，可能放大价格、压缩利润或造成信号与最终报价不一致。

**建议**：检测和风控阶段保留原始精度；只在构造最终订单时按实际 market tick 规范化一次；最终报价后重新计算净利润并决定是否下单。

### 12. `WINNER_BUY_MAX_PER_WINDOW` 实际限制扫描轮次而非订单/成交

- **涉及文件**：`src/main.rs:863-940`
- **严重等级**：中危（Medium）
- **问题**：计数器在扫描市场前递增。无信号也消耗额度；一次扫描中 BTC、ETH 同时有信号时又可能下两单。

**建议**：

- 分离 `scan_count`、`signal_count`、`order_attempt_count`、`fill_count`；
- `MAX_PER_WINDOW` 明确作用于实际订单或实际成交，并按 market slug 去重；
- 未成交后仅在信号仍满足更严格条件时允许有限重试；
- 加入一次扫描双市场同时触发、无信号后晚些出现信号、部分成交后是否重试等测试。

### 13. Winner Buy 与连接状态的可观测性不足

- **涉及文件**：`src/main.rs`、`src/runtime_metrics.rs`、日志配置
- **严重等级**：中危（Medium）
- **问题**：大量底层 WebSocket 错误没有进入 `stream_reconnects`；无信号、无 ask、价格不符等拒绝原因只在 debug 日志中，无法计算策略漏斗和阈值质量。

**建议**：增加结构化指标：

- 每资产扫描次数及各拒绝原因；
- YES/NO bid、ask、spread、深度、连续确认次数；
- 下单前和成交后的标的价格、基准价、TWAP、安全边际；
- requested、filled、average price、fee、资金占用；
- pending、resolved、redeemed、超时和查询失败数量；
- 底层 WS 断线、自动恢复、REST 降级和数据陈旧时长。

---

## 六、统一修复建议对照表

| 序号 | 模块 | 等级 | 核心问题 | 修复方向 |
| :---: | :--- | :---: | :--- | :--- |
| 1 | MarketScheduler | Critical | 预刷新可能重新进入旧窗口 | 明确查询下一窗口并做边界测试 |
| 2 | WindDown / Winner Buy | Critical | 持有至结算仓位可能被低价强平 | 仓位来源标签、过滤或配置互斥 |
| 3 | Winner Buy 结算 | Critical | 活动赔率被当作最终 1/0 | 校验 closed/resolved/可赎回状态 |
| 4 | Pending Winner | Critical | 待结算仓位随窗口销毁 | 进程级状态、journal 持久化、幂等恢复 |
| 5 | Winner Buy 策略 | Critical | 条件毛利润被当作独立 edge | 独立概率/TWAP 模型和净 EV 门槛 |
| 6 | PositionBalancer | High | 撤销赤字方对冲买单 | 仅抑制过剩方，保留补齐方 |
| 7 | PositionTracker | High | API 同步使敞口虚高 | 分离成本、名义敞口和最坏损失 |
| 8 | Winner Buy 入场 | High | 60 秒 TWAP 尚未形成便下注 | 缩短窗口、TWAP 安全边际、连续确认 |
| 9 | Winner Buy 风控 | High | 绕过资金账本、journal 和统一风险门 | 统一 reservation/commit/position 生命周期 |
| 10 | HedgeMonitor | Medium | 止盈止损状态竞态 | 单仓位原子状态机和幂等下单 |
| 11 | 报价精度 | Medium | 双重舍入和 tick 偏差 | 最终下单前只规范化一次并复算净利 |
| 12 | Winner Buy 次数 | Medium | 限制扫描轮次但可多下单 | 分离扫描/信号/订单/成交计数 |
| 13 | 可观测性 | Medium | 断线和拒绝漏斗不可见 | 结构化指标、数据新鲜度和真实结算日志 |

---

## 七、建议实施顺序与验收标准

### P0：先恢复结果真实性和持仓安全

1. 修复 Winner Buy 的真实结算判定。
2. 将 pending winner 持仓提升为跨窗口、可持久化状态。
3. 处理 WindDown 与 Winner Buy 的仓位生命周期冲突。
4. 修复 MarketScheduler 窗口边界逻辑。

验收标准：

- 活动市场绝不会增加 `winner_buy_wins/losses`；
- 市场结束后，只有明确 resolved 的 outcome 才产生 realized PnL；
- rollover 和程序重启不会丢失待结算仓位；
- 同一 fill 不会重复结算；
- Winner Buy 仓位不会被 WindDown 当作普通残腿低价卖出；
- 每个 5 分钟周期只进入正确窗口一次。

### P1：使 dry-run 具备真实资金约束

1. Winner Buy 接入 paper ledger、RiskManager、PositionTracker 和 journal。
2. 正确记录部分成交、成交均价、费用和资金占用。
3. 修正 PositionBalancer 和 PositionTracker 的风险计算。
4. 增加策略级、资产级和全局亏损熔断。

验收标准：

- 100 USDC 虚拟额度无法累计占用超过可用资金；
- 未结算 token 的成本始终占用现金或风险额度；
- 所有订单、成交、持仓、结算和赎回可以从 journal 对账；
- API 同步前后，同一组合的风险敞口不会无原因跳变。

### P2：重新定义 Winner Buy 入场策略

1. 接入与市场规则一致的基准价和 60 秒 TWAP 数据。
2. 建立 `p_model` 和净 EV 门槛，禁止只按 ask 高低认定优势。
3. 增加 TWAP 可反转边界、连续确认、spread 和深度过滤。
4. 修正每窗口次数语义和有限重试机制。
5. BTC、ETH 独立统计和校准。

验收标准：

- 每笔 signal 都能记录独立概率、有效成交价、费用和净 EV；
- `p_model <= effective_price + buffer` 时绝不下单；
- 能回放解释每次入场和拒绝原因；
- 不再把“高赔率”直接称为“胜负已定”。

### P3：完善执行精度、并发与可观测性

1. 修复 HedgeMonitor 状态竞态。
2. 统一 tick 舍入和最终报价复算。
3. 修复 WS 断线/重连指标。
4. 为 TUI 和日志增加 Winner Buy 漏斗、pending 和真实结算状态。

验收标准：

- 同一仓位不会产生重复平仓单；
- 信号利润与最终订单价格下的净利润一致；
- 连接异常、降级和恢复均有一致指标；
- TUI 的 estimated、mark-to-market、realized 三种收益互不混淆。

### 进入极小额实盘前的数据门槛

完成上述修复后，应重新开始 dry-run 样本采集，旧日志不可继续计入真实胜率。至少满足：

- 每笔成交都能与最终 resolution 对账；
- 样本覆盖 BTC、ETH、不同波动率和不同剩余秒数；
- 统计净收益已经扣除费用、滑点、部分成交和资金占用；
- 最大回撤、连续失败和按价格区间胜率均在预设风险范围内；
- 用置信区间证明真实胜率高于对应成交价格与安全缓冲，而不只是观察到较高的表面胜率；
- 在这些条件满足前，保持 `DRY_RUN=true`。

---

## 八、实施进度记录

### 2026-08-20：P0 已完成

本阶段完成以下修改：

1. **真实结算门槛**：Winner Buy 只有同时满足当前时间不早于市场结束时间、Gamma `closed=true`、outcome prices 接近终局 `1/0` 时才计入 realized PnL。活动市场的 `0.98/0.02` 不再被提前结算。
2. **跨窗口与跨重启持久化**：新增 Winner Buy 追加式状态日志，使用唯一 `position_id` 幂等记录 pending/settled；启动时恢复未决仓位，损坏的末尾记录会被隔离。paper/live 使用独立文件，避免模拟仓位和真实仓位混合。
3. **WindDown 动态保护与实盘互斥**：待结算 Winner Buy token 进入共享保护集合；WindDown 在 cancel、merge 和 sell 阶段排除这些 token/condition。保护集合为动态共享状态；此外，实盘配置禁止同时启用 Winner Buy 和定时 WindDown，从入口消除两项异步资金写操作之间仍可能存在的竞态。dry-run 可同时启用以继续观察信号。
4. **窗口调度修复**：提前刷新时固定查询预先计算的下一窗口 timestamp，不再在提前唤醒后按当前时间向下对齐到旧窗口。
5. **自动化测试**：新增活动市场不得结算、终局胜负、未来结束时间、journal 重启恢复与幂等、损坏尾部恢复、动态 WindDown 保护、提前刷新目标窗口等测试。

状态文件命名规则：

```text
results/winner-buy/paper/orders.ndjson
results/winner-buy/live/orders.ndjson
```

验证结果：

- `cargo test --all-targets`：通过；
- P0 新增定向测试：通过；
- `cargo clippy --all-targets -- -D warnings`：被 `src/ui/state.rs` 中已有的两项风格警告阻断（`unnecessary_map_or`、布尔值 `assert_eq!`），与本阶段修改无关，后续应在 P3 代码质量整理时处理。

### 2026-08-20：P1 已完成

本批次完成以下修改：

1. **Winner Buy 接入 dry-run 资金生命周期**：下单前按限价与费用缓冲预留虚拟现金；成交后按模拟盘口的实际部分成交量和有效均价扣减现金，未结算成本保持占用；明确终局后才按 `0/1` payout 结算并释放仓位占用。100 USDC 虚拟额度不能再被 Winner Buy 重复超额使用。
2. **重启恢复 paper 账本**：Winner Buy journal 回放会重建历史总投入、已实现盈亏、未结算成本和未结算仓位数；进程重启后，可用虚拟现金与历史交易状态保持一致。
3. **成交数据统一**：Winner Buy 执行结果新增 requested、filled、cost、average price 和 estimated fee。dry-run 使用实时 CLOB 深度计算实际成交均价，并把配置的 fee buffer 作为保守费用计入成本；live 路径使用交易所返回的 making/taking amount 计算实际成交份额与有效均价。
4. **接入统一风险门**：下单前通过 PositionTracker 原子预留最坏计划成本，成交后登记实际 token、份额和成本；同时复用 canary 的单笔、单窗口、进程累计名义金额与全局亏损预算。journal 写入失败会触发 fail-closed safety gate，阻止继续入场。
5. **修复 PositionBalancer**：YES 已过剩时只撤销继续增加 YES 的买单，保留补齐缺口的 NO 买单；NO 过剩时反向处理。挂单本身制造反向过量的原有分支保持不变。
6. **修复 PositionTracker API 成本同步**：优先采用 API `initialValue`，缺失时使用 `avgPrice × size`，两者都不可用时才按每份 1 USDC 保守回退；成对的 100 YES@0.45 + 100 NO@0.50 不再被无条件放大为 200 USDC 敞口。
7. **自动化测试**：新增 Winner Buy 现金占用/终局释放/重启恢复、canary 名义金额和亏损预算、PositionBalancer 双方向保护，以及 PositionTracker API 成本优先级测试。
8. **持久化策略/资产亏损熔断**：新增 Winner Buy 策略累计已实现亏损、单资产累计已实现亏损和连续亏损三道门槛。BTC、ETH 等资产分别累计；状态从 journal 重放恢复，重启不能绕过熔断。
9. **全链路 journal**：每次真正提交执行器前先记录 `attempted`；随后按结果记录 `no_fill`、`failed` 或 `pending`，终局记录 `settled`，胜出仓位继续记录 `redeem_submitted` 和 `redeemed`。旧版 `pending/settled` journal 保持向后兼容。
10. **赎回状态机与失败关闭**：dry-run 模拟 payout 的提交和确认；live 仅在 `WINNER_BUY_AUTO_REDEEM=true` 且代理钱包/Builder 凭据校验通过时自动调用链上赎回。赎回确认前仓位继续占用 PositionTracker 并保持 WindDown 保护；若提交后未确认，重启会识别为不确定写入、保持仓位并关闭安全门，禁止盲目重试。
11. **配置补充**：新增 `WINNER_BUY_MAX_STRATEGY_LOSS_USDC`、`WINNER_BUY_MAX_ASSET_LOSS_USDC`、`WINNER_BUY_MAX_CONSECUTIVE_LOSSES` 和 `WINNER_BUY_AUTO_REDEEM`。当前 `.env` 保持自动赎回关闭，dry-run 不受影响。

验证结果：

- `cargo test --all-targets`：106 项通过（lib 11、bin 95）；
- `cargo clippy --all-targets --no-deps -- -A clippy::unnecessary_map_or -A clippy::bool_assert_comparison -D warnings`：通过；
- `git diff --check`：通过；
- 不带豁免的严格 Clippy 仍仅被 `src/ui/state.rs` 中既有的 `unnecessary_map_or` 和布尔 `assert_eq!` 两项阻断。

P1 验收项现已关闭：虚拟资金不会超额占用；未结算或待赎回 token 持续占用资金/风险额度；订单尝试、执行结果、成交持仓、终局和赎回状态可以从 journal 重放；API 同步不再无条件将历史成本放大到 `$1/share`。

下一阶段进入 P2：引入与市场结算规则一致的基准价/TWAP、独立 `p_model` 和净 EV 门槛，并修正窗口次数语义、连续确认、spread/深度过滤及 BTC/ETH 独立校准。在 P2 完成并重新取得可信 dry-run 样本前，继续保持 `DRY_RUN=true`。

### 2026-08-20：P2 已完成

官方依据：

- 5 分钟 Up/Down 市场规则使用窗口起点与终点的 Chainlink 数据判断方向，而不是使用 Polymarket CLOB 赔率或任一交易所现货价：[Polymarket BTC 5-minute market rules](https://polymarket.com/event/btc-updown-5m-1785864300)。
- Polymarket 官方推荐通过 RTDS 读取 Chainlink 计算的 30/60 秒 TWAP；`payload.timestamp` 是 Chainlink observation time，RTDS 不提供历史快照或断线回放，并明确禁止在未知采样规则时自行复刻该 TWAP：[Chainlink TWAP Prices](https://docs.polymarket.com/market-data/chainlink-twap)。

本阶段完成以下修改：

1. **官方 Chainlink 60 秒 TWAP 数据源**：新增独立 RTDS WebSocket 服务，订阅 `crypto_prices_twap_sixty`，优先按 E18 `full_accuracy_value` 精确解析；每 5 秒发送应用层 `PING`，断线后重连和重新订阅。
2. **窗口基准价与数据真实性门槛**：按 Chainlink observation timestamp 保存 10 分钟历史；只接受与窗口起点时间偏差在配置范围内的 opening TWAP。程序中途启动、断线后无回放、价格过期、样本不足或时钟异常时全部 fail closed，不使用 Gamma/CLOB/第三方现货代替。
3. **独立概率模型**：用当前 TWAP 相对 opening TWAP 的方向性距离、近期 TWAP 变化率估计和剩余秒数计算保守 `p_model`。BTC、ETH 和其他资产分别配置最低波动率与模型风险缓冲，不再把 ask 本身当作胜率。
4. **可反转边界**：要求方向性 margin 大于 `固定最小 margin + reversal_sigma × 剩余时间波动标准差`；即使 CLOB 已显示 0.98，仍可能因为剩余时间内可反转而被拒绝。
5. **净 EV 门槛**：统一计算 `net_ev = p_model - effective_price - model_risk_buffer`；P2 最初使用固定 fee buffer，已在 2026-08-21 的 P4A 改为 CLOB V2 市场动态费率。只有 `net_ev >= WINNER_BUY_MIN_NET_EV` 才能进入执行流程。
6. **盘口质量过滤**：候选方向必须同时存在真实 bid/ask；spread 不得超过配置上限，卖一深度折算 USDC 不得低于最小深度。
7. **连续确认**：必须由若干个不同的 Chainlink observation 确认相同方向；主循环重复读取同一条 TWAP 不会增加确认数。P4A 已进一步将确认状态与瞬时盘口可执行性解耦：缺盘口、无信号、数据暂时不可用和盘口/EV 拒绝只暂停计数；方向变化、明确越过反转边界或 TTL 过期才重置。
8. **次数语义与有限重试**：`WINNER_BUY_MAX_PER_WINDOW` 改为统计真正进入执行器的订单尝试，不再在扫描前消耗；循环内每笔尝试后立即复核总额度，避免 BTC、ETH 同轮突破上限。新增每 market 的 `WINNER_BUY_MAX_ATTEMPTS_PER_MARKET`。
9. **可回放证据**：每个 admitted attempt 在 journal 中保存 opening/current TWAP、Chainlink observation timestamp、`p_model`、净 EV、方向 margin、反转边界、spread 和深度；拒绝路径输出同一组结构化诊断字段和明确原因。
10. **配置与 dry-run 默认值**：`.env`、`.env.example` 和 README 已加入 RTDS、freshness、opening skew、波动率、确认次数、margin、spread、深度、净 EV 以及 BTC/ETH 独立参数。当前仍为 `DRY_RUN=true`。

验证结果：

- `cargo test --all-targets`：111 项通过（lib 11、bin 100）；
- 新增测试覆盖 E18 RTDS 解析、缺失 opening、陈旧 feed、可反转方向、净 EV 入场、不同 observation 连续确认和订单次数语义；
- `cargo clippy --all-targets --no-deps -- -A clippy::unnecessary_map_or -A clippy::bool_assert_comparison -D warnings`：通过；
- `git diff --check`：通过。

运行注意事项：RTDS 明确不提供 snapshot/history，因此程序启动时若当前窗口已经开始且缓存中没有起点附近的 observation，该窗口不会执行 Winner Buy；应至少运行到下一个完整窗口再评估策略漏斗。P2 完成并不代表模型已被真实样本证明有正收益，仍需重新积累 dry-run 数据。

### 2026-08-20：P3 已完成

本阶段完成以下修改：

1. **HedgeMonitor 原子关闭状态机**：用显式 `Open → Closing → Resting/Failed` 状态替代特殊字符串 `"processing"`。同一 `pair_id` 的 `Open → Closing` 在 `DashMap` entry guard 内完成，多个并发行情回调只能有一个取得下单权。
2. **不确定写入 fail closed**：签名器在提交前创建失败可安全回到 `Open`；构建、签名或提交链路返回无法区分阶段的错误时进入持久化 `Failed/ManualReview`，禁止自动重试，避免网络响应丢失后重复卖出。重启时遗留 `Closing` 同样恢复为 `Failed`。
3. **恢复订单状态一致性**：活动恢复卖单统一标为 `Resting`，明确取消后才回到 `Open`；已有 resting order 时即使对向腿晚到，也不会直接删除监控并遗留无人管理的卖单。
4. **保留检测精度**：套利 detector 不再 `round_dp(2)`，从原始 best ask、深度、最小订单额到风控均保留 wire precision，不会因四舍五入制造虚假 edge 或隐藏真实 edge。
5. **最终 tick 与净边际唯一裁决点**：执行器继续在加入滑点后按每个 token 的实际 tick 向上规范化买价，随后重新计算 `1 - YES - NO`，并同时覆盖最小利润、执行 spread 和 fee buffer；不满足时在任何订单构造前拒绝。
6. **WS 连接指标闭环**：公共盘口流创建失败、用户流认证/订阅失败和运行中断线均进入 reconnect 指标并采用有界指数退避；成功恢复另计 `stream_recoveries`，首次正常连接不会误计恢复。
7. **Winner Buy 漏斗指标**：新增 scans、signals、no-book、no-signal、模型数据/盘口/反转边界/净 EV 拒绝、等待连续确认、attempts、fills、pending、resolved、redeemed 和 settlement-query-failure 指标。`pending` 使用 gauge，并在启动恢复、成交和结算后同步。
8. **TUI 生命周期展示**：RISK 面板分开展示 `scan/signal/attempt/fill`、模型拒绝/等待确认、`pending/resolved/redeemed`、estimated PnL 与 realized PnL，避免把条件毛利和真实结算收益混为一谈。
9. **代码质量收口**：清理原有 TUI/Winner Buy Clippy 警告，严格 `-D warnings` 不再需要豁免。

验证结果：

- `cargo test --all-targets`：114 项通过（lib 11、bin 103）；
- 新增测试覆盖同一仓位关闭权只能取得一次、模糊关闭状态重启后 fail closed、原始精度不会产生虚假套利，以及连接恢复/漏斗指标；
- `cargo clippy --all-targets --no-deps -- -D warnings`：通过；
- 最终执行报价已有回归测试覆盖滑点吃尽 edge、按实际 tick 向上对齐和每腿名义金额限制。

P0～P3 的代码整改已完成，但这不等于已经证明策略可盈利或已经通过真实资金验收。下一步应进入 **P4 验证阶段**：重新运行完整窗口 dry-run，按新增漏斗校准数据新鲜度、反转边界、净 EV 与深度阈值；确认持续运行、重启恢复和终局结算样本均正常后，再执行极小额 canary。

### 2026-08-21：P4A 日志驱动修复已完成

依据约 15 小时 50 分钟 dry-run 日志（10,016 次扫描、9,764 次 no-book、59 个候选但 0 次 attempt/fill）完成以下整改：

1. **盘口主数据源改造**：Winner Buy 优先复用已订阅的 SDK 公共 WebSocket 盘口缓存，不再每秒对两个 outcome 发起 REST 请求。只有尚未收到 WS snapshot 时才回退 SDK 的 CLOB REST `order_book`，并复用统一的瞬时读取重试策略。
2. **REST 错误不再吞掉**：YES/NO 回退失败分别输出带 token 和错误的 debug 日志，并计入 `winner_buy_book_read_errors_total`；不再用 `unwrap_or_default()` 把传输错误伪装成空盘口。
3. **合法单边盘口保留**：top-of-book 的 bid、ask 和 ask size 改为独立可选字段。双边都没有报价才算 no-book；只有 ask 的尾盘盘口仍可用于方向候选识别，但进入 spread/深度模型前若缺 bid 会明确计入 incomplete-book 并暂停。
4. **确认状态与执行条件解耦**：瞬时缺盘口、无候选、数据缺失、spread/depth 或净 EV 不通过不会抹掉已取得的独立 Chainlink observation。方向反转、明确不满足 reversal boundary 或超过 `3 × WINNER_BUY_FEED_MAX_AGE_MS` 的 TTL 才重新从一次确认开始。
5. **CLOB V2 动态费率**：每个 condition 在窗口开始时读取 `/clob-markets/{condition}` 的 `fee_details` 并缓存；每份费用按 `rate × [price × (1-price)]^exponent` 计算。该费用同时进入模型 effective price、计划资金预留和 dry-run 成交成本。费率读取失败时 fail closed 跳过该市场，不再以固定 `ARBITRAGE_FEE_BUFFER` 代替 Winner Buy 手续费。
6. **模型拒绝细分**：除原有 data/market/boundary/EV 汇总外，新增 no-price-history、stale-price、no-opening-reference、invalid-price、insufficient-observations 独立计数；日志保留 asset、slug、窗口、价格、fee、概率和净 EV，能够直接定位漏斗阻塞点。
7. **连接状态闭环**：RTDS 新增连接、断开、连接失败、有效 observation 数及最后 observation timestamp；30 秒无任何消息会主动重连。SDK 公共市场流和认证用户流均通过 SDK 暴露的内部 connection state 统计 reconnect/recovery，市场流另记录最后收到的盘口 timestamp；任一流未连接时 Winner Buy 暂停入场，不再只依赖外层 stream 是否结束。
8. **反事实评估 journal**：每个能够完成模型计算的候选（包括被拒绝、等待确认、无成交）写入独立追加式 NDJSON，记录唯一 evaluation id、完整模型和盘口输入、动态费用及 rejection。终局关闭后按 slug 自动补写实际方向和每份反事实净盈亏；重启会恢复尚未终局的 evaluation，避免只分析 admitted/fill 样本造成幸存者偏差。
9. **调试配置**：本地 `.env` 已改为 `RUST_LOG=debug,polymarket_client_sdk::serde_helpers=error`，用于下一轮 VPS dry-run 获取逐候选诊断，同时屏蔽 SDK 对已知 Gamma 扩展字段产生的大量重复告警。debug 会显著增加日志量，现有 50 MB × 10 的轮转配置应继续保留。

反事实文件命名规则：

```text
results/winner-buy/paper/evaluations.ndjson
results/winner-buy/live/evaluations.ndjson
```

关于“把 REST 换成 SDK”：原实现的 `winner_book_top()` 已经使用 `polymarket_client_sdk_v2`，SDK 的 `order_book()` 底层仍是 HTTP REST，因此单纯更换调用封装不会改善网络质量。本阶段采用的是更有实际作用的方案：**SDK WebSocket 缓存为主、SDK REST 重试回退、错误分类与连接状态指标**。dry-run 成交模拟仍会在真正尝试时读取一次实时 REST 深度，以避免用过期缓存模拟成交。

本阶段没有调整 price、margin、reversal sigma、确认次数和净 EV 阈值。应先用新数据链路运行至少 24 小时并取得可终局对账的反事实样本，再决定参数变化。
