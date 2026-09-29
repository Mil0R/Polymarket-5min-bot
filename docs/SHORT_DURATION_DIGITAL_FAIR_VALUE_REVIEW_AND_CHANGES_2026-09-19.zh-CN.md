# Short Duration Digital Fair Value：报告复核与本轮改进

本轮复核对象是 `SHORT_DURATION_DIGITAL_FAIR_VALUE_REASSESSMENT_2026-09-19.zh-CN.md`。结论仍是：008 中的模拟成交由旧的 Gaussian/floor 公式产生，冻结的经验模型只参与离线评分；正的模拟总 PnL 不能证明冻结模型盈利，更不能视为可执行实盘收益。当前经验模型在可报价样本上的 Brier 分数高于市场中价（表现更差），暂不切换交易决策模型。

## 已改进

1. 新增只读研究模型推理器，启动时校验模型文件 SHA-256 和资产/检查点是否齐备。`model_observation`、`opportunity`、延迟后拒单及 `shadow_fill` 分别标出 `decision_model=gaussian_floor_v1` 和研究模型的哈希、选中类型、概率；研究模型**不触发下单或 shadow fill**。经验残差使用与离线研究相同的 add-one 规则；floor 的价格边际采用离线脚本使用的普通收益率，而非经验模型使用的对数收益。
2. 离线冻结评估对新日志中的线上研究概率做逐条一致性检查；模型身份或概率不匹配时失败。旧 008 事件未记录研究概率，所以结果会明确显示 `NOT_RECORDED`，不能被误写成已经通过线上一致性校验。
3. 可选每秒记录双边 WebSocket 买卖盘、前 20 档深度、盘口时间戳、结算参考流的当前值及 30/60/90 秒锚点；REST fallback 也记录成对快照 ID、延迟与盘口。首次模型观测保存近 90 秒 TWAP 路径。首次信号后的市场不再重复产生交易机会，但继续保存 WebSocket 盘口轨迹供拒单后固定重试规则的离线研究。超过 20 档的快照会显式标 `levels_truncated`，不可冒充完整可执行深度。
4. 008 冻结入口的 `status`、已有事件的 `start` 和 `finalize` 都能审计实际覆盖、缺失 UTC 日、重复观测，并输出事件文件 SHA-256 指纹。该指纹没有与预存基线比较，不能独自证明文件未被替换；跨机同步仍需使用严格前缀检查/备份流程。`finalize` 在原始配对数量/日期不足时不会继续输出声称完成的冻结研究。最终概率门还要以带官方结算标签的可用样本为准。

本地 008 当前审计：807 条原始模型观测，BTC/ETH/SOL 原始配对数为 44/24/42，每资产只覆盖 2026-09-18、19 两个 UTC 日；冻结门要求每资产至少 75 条配对和 5 个 UTC 日。9 月 14–17 日无事件。旧事件文件不会被本轮代码补齐。

## 新研究批次运行

把本轮编译的 `short_duration_digital_fair_value`、脚本及冻结模型文件一同部署后，在项目根目录运行；若在服务器自行编译，先执行 `cargo build --release --bin short_duration_digital_fair_value`：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_model_audit.sh dry-run research-model-audit-002
```

默认 30 秒检查点、每 1 秒保存一次研究盘口快照；日志与结果分别放在：

- `logs/short-duration-digital-fair-value/research-model-audit-002-5m-30s/system.log`
- `results/short-duration-digital-fair-value/research-model-audit-002-5m-30s/events.ndjson`

`report` 和 `calibrate` 可用同一脚本的首个参数运行。盘口快照会明显增大结果文件；需要降频可在命令前设置 `SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_SNAPSHOT_INTERVAL_MS=2000`。这仍是旧公式的 dry-run 决策加并行研究预测，**不是新模型的收益测试或新的冻结批次**。

`research-model-audit-002` 修正了 001 的三个观测问题：精确 3¢ 价差不再因浮点误差被误拒；REST 请求成功与双腿有效报价分开记录（`fetch_success`、`paired_book_complete`、`book_usable`），其中“可用”仅指新鲜且结构有效，**不代表**已通过深度、价差、费用或成交检查；研究模式额外订阅 Chainlink 原始现货价和 Binance 价格流，在 `reference_price_observation` 里同时保存价格源时间、发布端时间和本地接收时间，并在盘口快照中保存按本地接收时间的 `spot_asof`。首次模型观测和机会还保存 90 秒 `spot_path`。原始现货价只供离线研究，不替代 `crypto_prices_twap_sixty` 结算流，也不改变任何 dry-run 决策。001 原始日志保持不变，脚本拒绝将新的 dry-run 事件追加到该旧批次。

运行后先检查 002 的 `reference_price_observation` 是否同时覆盖 BTC/ETH/SOL 的原始现货价、TWAP，及 `research_market_snapshot` 中 REST 双腿有效率；如果价格流缺失，不应把 `spot_asof: null` 当作零价，也不应进入下一阶段的模型收益比较。

现有 008 可用 `bash scripts/run_digital_fair_value_frozen_008.sh status` 查看覆盖和日志 SHA。数据缺口不能通过改参数或重算报告修复。

## 尚未解决的研究门槛

- 002 开始保存原始现货路径，但尚未证实它对最终结算结果存在稳定、可交易的领先关系，也未证实滚动 TWAP 的具体加权规则。不能据此声称已建立条件化终值模型。
- 新盘口日志可分析信号前后变化，但 WebSocket 双腿非原子快照、最多 20 档、无真实订单回执；它不能单独证明替代模型的真实成交和净收益。必须按时间戳、深度截断与数据年龄过滤，再做固定延迟和失败率的反事实回放。
- 先在新的可报价样本上逐资产、逐日超过市场中价的概率评分，并在保守费用/滑点/延迟假设下得到正净值，再考虑新模型的模拟决策或独立冻结批次。未达到条件前不扩大预算、不启用真实订单。
