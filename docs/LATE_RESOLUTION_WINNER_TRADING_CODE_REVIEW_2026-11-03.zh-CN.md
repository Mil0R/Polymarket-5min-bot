# Late Resolution Winner Trading 代码审核与优化建议

> 后续实现：已按逐条复核后的采纳范围完成工程修复和离线审计工具，详见 [审核建议实现记录](LATE_RESOLUTION_WINNER_REVIEW_IMPLEMENTATION_2026-11-03.zh-CN.md)。下文保留原始审核记录；其中时间偏差方向、REST 超时、熔断政策、Paper 压力假设及模型参数建议，以实现记录中的修订为准。

日期:2026-11-03。本文只是审核记录,没有修改任何代码,也没有运行测试。

审核范围:

- [late_resolution_winner_model.rs](../src/late_resolution_winner_model.rs)
- [late_resolution_winner_trading.rs](../src/late_resolution_winner_trading.rs)
- [app.rs](../src/app.rs) 里的扫描和下单循环
- [executor.rs](../src/trading/executor.rs) 的 `buy_winner`
- [config.rs](../src/config.rs) 的校验

结论来自代码和仓库里已有的研究文档。和交易所实际行为有关的几条(费用口径、订单类型)没有实测,已标注"需验证"。

## 背景

这个策略赢一次赚约 0.03,输一次亏约 0.97,盈亏平衡胜率约 97.2%。文档里 305/312 的胜率(约 97.8%)几乎贴着平衡线,Wilson 下界还低于平衡线。所以任何高估概率的地方都会直接吃掉利润。

## 一、高优先级逻辑问题

### 1. 模型输入时间是过期的,会让概率偏乐观

- [`now`](../src/app.rs#L1507) 和 [`seconds_until_end`](../src/app.rs#L1577) 在每秒 tick 开头取一次。
- 之后经过 REST 补读(最长 3s)、前面几个市场的串行处理,才到 [`evaluate(now_ms…)`](../src/app.rs#L2433)。
- 结果是 TWAP 年龄和剩余时间都被低估。
- `seconds_until_end` 用 `num_seconds()` 向下取整,剩余 1.9s 会被当成 1s,σ√T 被低估约 30%。
- 模型里的 [`remaining = seconds_remaining.max(1)`](../src/late_resolution_winner_model.rs#L398) 没有把 TWAP 样本自身的年龄加进去。`FEED_MAX_AGE` 默认允许 5s,T=10s 时有效视野会错 50%。
- 建议:在评估前和下单前重新取 `Utc::now()`,用毫秒精度的剩余时间,并用 `T_eff = T + feed_age` 计算 σ√T。

### 2. 方差模型很可能没校准,实际靠波动率下限在撑

- σ 来自 1 秒 TWAP 增量的均方根再乘 √T([`volatility_bps_per_sqrt_second`](../src/late_resolution_winner_model.rs#L1220))。
- 60 秒 TWAP 的相邻增量高度相关,逐秒波动不等于终点风险。`docs/LATE_RESOLUTION_WINNER_TRADING_FLOW_AND_MATH.zh-CN.md` 第 10 节也承认这一点。
- 实际起作用的是 BTC 0.8 / ETH 1.2 bps/√s 的下限。
- 正态尾部在 z≥3 时给出 p≈0.9996,但真实行情有跳变和 Chainlink 更新间隔,尾部更肥。
- 固定 1%~2% 的风险缓冲和这个尾部不匹配。
- 建议:
  - 沿用仓库已有的 `terminal/residual study`,直接按"剩余秒数 × 资产"估计 `A_end − A_now` 的经验分布。
  - 或者用 isotonic 映射把 p 校准到实测胜率。
  - 至少把 p 封顶(例如 0.995),并让风险缓冲随 z 和 T 变化。

### 3. 开盘参考没有被验证,容差也偏大

- [`opening`](../src/late_resolution_winner_model.rs#L364) 取窗口起点 ±5s 内最近的样本。
- 这个偏差可能和 3bps 的最小 margin 同量级。
- 代码里 [`reference_verified:false`](../src/app.rs#L2548) 是硬编码的,说明"开盘 TWAP / 结算源 / Up-Down token 顺序"至今没有逐市场核对。
- 建议:
  - 离线用已结算的 Gamma 结果核对"开盘 TWAP 与收盘 TWAP 的符号是否等于结算结果"。
  - 把开盘容差收紧到 ≤1s,必要时插值。
  - 记录 `opening_skew_ms`,超过阈值直接拒绝。

### 4. 实盘下单没有对账,订单类型也没有强制

- 策略复用 `BINARY_COMPLEMENT_ARBITRAGE_ORDER_TYPE`。
- [config.rs](../src/config.rs#L835) 只在 canary 模式强制 FOK/FAK。
- 非 canary 的 live 如果配成 GTC/GTD:
  - [`buy_winner`](../src/trading/executor.rs#L709) 不会撤剩余挂单,也不会像套利路径那样调用 `freeze_and_reconcile_fill`。
  - `filled=0` 会被记成 `NoFill`,但挂单仍在,之后可能成交,形成没有记账的敞口。
- 需验证:live 返回的 `estimated_fee` 是 0,`cost = making_amount`。如果 V2 的手续费是从收到的份额里扣,那么 `filled*(1-price)` 的结算盈亏和赎回份额会对不上。
- 建议:winner 路径在所有 live 模式都强制 FOK/FAK,成交后用 trades 或余额对账。

### 5. 熔断设计不能真正限制亏损

- [`check_risk`](../src/late_resolution_winner_trading.rs#L1037) 只在下单前检查已结算的亏损。
- 单笔上限 `MAX_ORDER_USDC=50` 大于策略亏损上限 20 和单资产上限 10。一笔亏损就能越过上限,熔断只能事后停手。
- 未结算持仓的最坏亏损不计入。5 分钟窗口加上 Gamma 的结算延迟,可能叠加多笔未结算亏损。
- [`strategy_loss`](../src/late_resolution_winner_trading.rs#L1443) 是累计毛亏损,盈利不抵扣,也不衰减,重启回放后依旧保留,最终会永久停机。
- 建议:
  - 改成"已实现净盈亏回撤 + 未结算最坏损失"的滚动窗口限额。
  - 单笔最大亏损应小于熔断阈值。
  - 提供人工复位或自动恢复的时间机制。

### 6. 待结算持仓没有超时处理

- [`settle_late_resolution_winner_trading`](../src/late_resolution_winner_trading.rs#L844) 要求两个价格都达到 ≥0.999 / ≤0.001。
- 遇到争议、作废(50/50)或 Gamma 延迟,会无限返回 `None`。持仓、`PositionTracker` 预留的敞口和 paper 现金会一直占用,没有告警或兜底。
- 建议:增加最大等待时长和告警,并用链上 `payoutNumerators` 作为兜底判定。

## 二、中优先级

- **候选选择是"先到先得"。** [`max_per_window=1`](../src/app.rs#L2141) 对所有资产全局生效,谁先通过确认谁下单,不看 EV。建议同一个 tick 收集所有候选,选 `net_ev` 或 z 最大的一个。BTC/ETH/SOL 高度相关,保持"每窗口一笔"是合理的。
- **一次 FOK 失败会用掉整个窗口的名额。** 名额在 `record_attempt` 时就扣了。无敞口的 `NoFill` 应该允许有限次重试(2~3 次),并且每次都完整重评。
- **连续确认的信息量有限。**
  - 三次确认只看"不同的 TWAP 时间戳",而 1 秒 TWAP 样本高度相关,近似于一次观测加 2 秒延迟。
  - `reject()` 在若干 `continue` 路径上没有调用,例如 journal 写失败、`size<=0`、风控拒绝。
  - WS 或 user-WS 断开时也不清状态,TTL 是 15s。
  - 建议改成"z/margin 持续满足 + 价格未大幅漂移",或降为 1~2 次并提高 z 门槛。
- **新鲜度用的是发布方时间戳。** [`effective_sample_age_ms`](../src/late_resolution_winner_model.rs#L43) 允许最多 10s 的未来时间戳,并把负年龄夹成 0,等于把超前样本当成"最新鲜"。建议改用 `twap_reference_timing` 里记录的本地接收时间判断新鲜度。
- **paper 对薄盘口有系统性偏差。** `size = min(现金上限, ask_size)`,在 FOK 加 0.75 深度折扣下,当 `size==ask_size` 时 `available = 0.75*size < size`,永远不成交。这会让 paper 和 shadow 的成交率偏低,和实盘不一致。
- **进入评估的价格信息不完整。** 评估只用了最优价一档的深度(`ask*ask_size`)。
- **波动率没有异常值处理。** 重连造成的长间隔或单次跳变会被直接纳入均方根。

## 三、效率优化

1. **热路径上有同步 fsync。**
   - [`DurableNdjsonWriter::append`](../src/late_resolution_winner_trading.rs#L542) 每次都 `sync_data` 加 `available_space`。
   - 每个市场每秒的扫描会写 evaluation、confirmation、sensitivity 三条,全部发生在 async 线程上,下单前阻塞。
   - 建议:只保留 `attempt/pending` 的同步写,研究类日志交给后台线程批量写入。
   - 另外,目前研究日志写失败会 `continue`,等于研究盘出问题会连生产交易一起停掉。
2. **tick 是固定 1 秒轮询。** 决策延迟是 0~1s 加上约 2s 的确认。建议改为盘口和 TWAP 更新事件触发,或在最后 30s 用 200ms 的 ticker。
3. **市场串行处理。** REST 补读(最长 3s)和 `buy_winner`(paper 里还有 250ms 的 sleep)都在 for 循环里 `await`,其他资产的扫描被阻塞。建议并发处理各市场,或把下单丢到独立 task。
4. **`evaluate` 重复计算。** 每次调用都要复制 120s 的历史样本、算 4 个 horizon 的审计波动率,一个 tick 内每个资产、每个方向都重算一遍。可以按"资产 + 最新 TWAP 时间戳"缓存,审计字段延迟到需要落盘时再算。
5. **自动赎回默认关闭。** `AUTO_REDEEM=false` 时资金一直锁到手动赎回,上线前需要确认。

## 四、建议顺序

1. 先修第一节的 1(时间)、4(订单类型和对账)、5(熔断)、6(结算兜底)。这几项改动小,风险收益比最高。
2. 再做 3 的离线验证和 2 的经验校准,用已有的 journal 数据就够。
3. 最后做效率改造(异步日志、事件触发、并发)。

## 五、后续可做的事

写一个只读的校准脚本,用现有 `evaluations.ndjson` 检查模型概率 p 和实际胜率的偏差,以及开盘偏差对结果的影响。
