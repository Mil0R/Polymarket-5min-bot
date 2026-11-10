# Winner Trading 审核建议实现记录

日期：2026-11-03。对应 [原始审核](LATE_RESOLUTION_WINNER_TRADING_CODE_REVIEW_2026-11-03.zh-CN.md)。本次只修改本地代码、增加离线工具并测试；没有部署、修改服务器配置、重启服务或发送真实订单。工作区原有改动保留。

## 已落实的修复

|范围|实现与行为|
|---|---|
|定时扫描|`select!` 使用窗口外创建的持久 `interval`，丢弃错过的 tick，盘口事件不再重置扫描期限。保持 1 秒周期，不未经测量改成 200ms。窗口切换使用检查当时的时间。|
|模型时间|输入改为毫秒剩余时间；评估和提交前分别刷新时钟，预测时长包含源样本年龄。过期直接拒绝，提交签名及持久化后再次检查有效期限。Paper 延迟执行到期也不能提交现金。敏感性分析使用同一有效时长。|
|源和传输新鲜度|运行时要求匹配当前源样本的本地接收时间；同时检查源数据年龄与接收年龄。未来源时钟容差不再绕过接收时间检查。历史回放明确不要求当时未采集的接收时间。|
|开盘、方向与 token|保留已有开盘偏差审计字段；发现器、Gamma 结果都按 Up/Down 标签匹配。实盘结算及赎回使用 CTF collection/position 查询，从实际 token id 确认合约结果索引，再读取 payout，避免将数组顺序当成链上证明。|
|订单类型|所有 live winner，包括非 canary，在配置和执行器两处要求 FOK/FAK。Paper 的订单类型语义保留。|
|不确定执行|增加 `submitted/uncertain` 状态。POST 前持久化签名订单哈希；响应丢失、超时、未最终确认或对账歧义均保留完整风险预算并锁存安全开关。不会作为普通失败或 no-fill 自动重试。|
|成交、费用与恢复|启动时和暂停后的检查会按订单 id 查询订单、关联交易及链上 receipt；只接受最终确认、可归属的成交。现金成本和净到账份额从 ERC20/ERC1155 转账核对，支持 TransferSingle/Batch；部分成交、份额扣费均按净份额核算。混合其他持仓的歧义 receipt 保留待对账。赎回等待不确定状态同样跨重启保留。|
|历史数据迁移|旧版模糊 `failed` 和孤立 `attempted` 作为待核对记录恢复。新增明确的 paper 模式标记；dry-run 启动只释放明确标记为 paper 的中断尝试，不清除 live/legacy 不确定执行。|
|亏损预算|保留累计毛亏损和连续亏损锁存。下单额额外受“策略/资产剩余亏损预算－pending 最坏损失－不确定订单计划成本”约束，并在写 attempt 时以同一锁原子复核。已结算但待赎回资金继续受 PositionTracker 的资金预算约束。|
|人工复位|提供独立离线命令，要求审计理由；有 pending、不确定执行或不确定赎回时禁止复位。先取得状态 journal 排他锁，再回放及修复尾部，防止操作命令改写运行进程的日志。复位事件持久化，重启不会自动解除熔断。|
|结算|增加等待告警，默认到期后 600 秒；没有终态不会释放敞口。Live 以 CTF payout 为准；Paper 正常二元终态仍可用 Gamma，超时后也查询 CTF。支持 0～1 分数兑付；即使部分兑付后净亏损，仍保留赎回生命周期，按实际 payout 更新现金与亏损预算。|
|候选选择|一个扫描轮次收集确认完成的候选，以净 EV × 预算/深度限制后的份额排序；提交前再次检查模型、行情年龄、连接、窗口名额与风险预算。保留窗口、市场及 canary 尝试次数限制。|
|I/O 与并发|REST 补读最多 8 个并发，整个读取设真实超时；Paper/Shadow 延迟读取也有超时。下单由窗口任务管理器管理，执行等待不再串行阻塞其他候选扫描。Gamma 终态查询也有总超时。|
|日志|正式订单生命周期全部保持同步持久化。Live 研究日志使用有界后台队列、批量同步写并在正常关闭时排空；研究日志失败告警，不代替订单状态写入保护。要求完整记录的 Paper 研究继续等待持久化成功，但 fsync 放到阻塞线程池，失败时拒绝本次研究入场。|
|模型开销与缺口|只缓存历史波动统计和 horizon 审计值；缓存按样本值及实际历史区间失效，不缓存盘口、费用、剩余时间或最终决策。超过 5 秒的源样本间隔需要重新积累连续历史；连续数据中的真实价格跳变不裁剪。|

新增配置：`LATE_RESOLUTION_WINNER_TRADING_SETTLEMENT_ALERT_SECS=600`。`AUTO_REDEEM=false` 保留；live 启动会明确告警资金需人工赎回。

`estimated_fee` 在 live 中为现金手续费加份额手续费的成交价等值审计数；现金账本使用额外持久化的精确 `entry_cost`，结算使用净份额，分数兑付按抵押品的 1e-6 最小单位向下取整。旧记录没有 `entry_cost` 时才从份额和均价恢复成本。不要把费用审计数再次加到已核对成本。

## 保留为研究的策略调整

以下建议经过调整，不在本次直接改变生产策略：

- 不把模型概率封顶改为 0.995，不根据 z/T 随意拟合风险缓冲，不直接降低确认次数；现有数值和开盘 ±5s 容差保留。
- 不用滚动净亏损替代累计毛亏损，不自动恢复熔断，不放开 no-fill/失败重试。
- 保留最优价一档深度限制和 Paper 0.75 流动性压力折扣。薄盘口在延迟后不变时 FOK 可能失败，是压力假设；没有延迟完整盘口，不能宣称其与实盘成交率相同，也不为提高模拟收益取消折扣。
- 不因缺少更多实测就扩大盘口层数、加速到 200ms 或开启自动赎回。

新增只读工具：`scripts/late_resolution_winner_calibration_audit.py`。它记录输入 SHA256，保留拒绝/未成交候选，以每资产每市场最早有效评估去重；选择样本时不看结算标签，缺少该评估标签时也不换成后来的成功样本。输出概率分组、Brier、观察胜率、波动率下限比例及开盘偏差分组。Wilson 区间仅作描述，不把相关市场当作独立验证。

```bash
python3 scripts/late_resolution_winner_calibration_audit.py \
  /path/to/evaluations.ndjson --output /path/to/new-audit.json
```

可选 `--references /path/to/exact-boundary-twap.ndjson`：每行包含 `slug`、`opening_twap`、`closing_twap`、`opening_observed_at_ms`、`closing_observed_at_ms`、`window_start_ms`、`end_ms`、`reference_topic`。只有 topic 为 `crypto_prices_twap_sixty` 且起止样本恰好落在窗口边界时，才做开盘/收盘符号核对及终点残差分布统计。平价结果需要明确市场规则，不擅自标成 Up 或 Down。

已有历史修复版 journal 的 [示例审计](late-winner-calibration-audit-2026-11-03.json)：282 个首个有效观测，274 胜、8 负；平均模型概率约 0.6353，观察胜率约 0.9716，波动率下限比例 100%。这是旧模型候选的开发诊断，包含拒绝候选，**不是新代码回测、真实成交收益或 007 前瞻结果**。原始版本含损坏行，脚本明确拒绝；没有静默跳过或修改输入数据。

进一步使用本地既有 `server-low-latency-005-5m/reference-audit.json` 中 Gamma `priceToBeat/finalPrice` 元数据，得到 [参考价及残差诊断](late-winner-gamma-reference-audit-2026-11-03.json)：165 个可匹配市场，官方起止价符号与结算标签没有冲突；用日志中的开盘价配合官方终值计算方向，也没有出现符号冲突。开盘价最大绝对偏差约 **0.2400 bps**，中位数为 0。这个样本支持保留容差并继续验证，不能据此断言所有未来窗口均无开盘参考风险。

该缓存包含明确的 Chainlink TWAP 和 Up ≥ 比较规则、关闭状态及到期后抓取时间。工具另外输出按资产/时间桶分组的经验终点残差；每格只有 2～80 个独立市场，尚不足以校准极端尾部。例如 ETH 21～40s 组仅 49 个样本，观察到的上尾残差约 21.23 bps，不应直接将小样本分位数作为生产风险阈值。

```bash
python3 scripts/late_resolution_winner_calibration_audit.py \
  /path/to/evaluations.ndjson \
  --gamma-reference-audit /path/to/reference-audit.json \
  --output /path/to/new-reference-audit.json
```

Gamma 元数据核对和经验残差诊断已经执行；精确边界 TWAP 原始采样的独立核验仍缺证据，输出没有混用这两种来源。概率映射拟合与生产参数调整仍需独立的时间切分、按市场/日期分组和结算标签可用时间约束；不能把旧数据开发诊断当成前瞻通过。

## 离线操作命令

均要求策略进程已停止，journal 排他锁会拒绝并行操作。路径和原因由操作者明确指定；本次没有执行这些写命令。

```bash
# 审计完成且没有 pending/不确定状态后，持久化重置累计亏损及连亏保护。
cargo run --bin late_resolution_winner_trading -- \
  reset-risk /path/to/orders.ndjson "已完成风险复核，原因及工单编号"

# 对旧版缺少订单哈希的记录，附上经外部核对的订单 id；下次 live 启动继续真实对账。
cargo run --bin late_resolution_winner_trading -- \
  attach-order /path/to/orders.ndjson POSITION_ID ORDER_ID "外部核对依据"

# 仅在外部确认订单已终止且零成交后，人工关闭未知尝试；不是自动超时恢复。
cargo run --bin late_resolution_winner_trading -- \
  resolve-no-fill /path/to/orders.ndjson POSITION_ID "终止订单及零成交核对依据"
```

新增 journal 字段均有 serde 默认值，旧记录可读取；新增状态和复位事件应使用新版本读取。若运行新代码采集研究数据，需以新 run、可执行文件哈希及方案版本标记时间/调度行为变化；不修改既有 005/006/007 journal 和冻结计划，不把新旧运行混作同一不变策略。

## 验证与边界

验证结果：`cargo check --all-targets` 通过；全库 Rust 单元测试 415 项通过、1 项默认忽略，该跨语言测试另外显式执行也通过；相关 Python 测试 35 项通过；`git diff --check` 通过。覆盖毫秒预测时长、源/接收时间双重检查、断线预热、反向 token 数组、持续盘口下定时扫描、并发预算、防止重复占用预算、跨重启未知执行、分数兑付、人工复位、旧失败迁移、后台日志排空和 receipt 转账解析。

额外执行 `cargo clippy --lib -- -D warnings`：本次新增的告警已处理，严格检查仍被 13 处既有告警阻塞，涉及已有 fair-value 函数参数数量、`digital_fair_value` 表达式及其他研究模块；没有为通过本次检查而修改这些无关实现或添加全局 lint 豁免。

测试使用本地数据和构造的转账日志。没有发送真实订单，也没有宣称 V1/V2、EOA/Safe/Deposit Wallet 的每一种成交与赎回组合均已完成实盘验收。链上读取失败、费用/金额无法归属、订单缺失或确认状态不明时，继续保留风险预算并要求对账，不以乐观估计补齐。
