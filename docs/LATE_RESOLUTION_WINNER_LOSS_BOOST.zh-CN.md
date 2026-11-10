# Late Resolution Winner：独立亏后加码 PAPER 验证

新批次 `server-loss-boost-001` 沿用候选轨道的报价门槛（BTC 0.96–0.98、ETH 0.96–0.97）、最后 60 秒及两次独立 TWAP 确认、含费预算和延迟模拟撮合。008 的冻结候选和 Operator 继续运行在原进程及原账本上。

- BTC、ETH 初始每笔上限均为 5 USDC。独立模拟本金 100 USDC，与原候选注册本金相同。
- 仅当本轨道的已成交订单确认结算、净盈亏小于零，才把该资产后续订单上限提到基准的 10 倍。BTC 与 ETH 各自计时；一侧亏损不调整另一侧。
- 从确认时间起持续 3600 秒。同资产再次确认亏损会重新计时一小时，不叠乘。未成交、执行错误、待结算和盈利不触发加码。
- 到期自动恢复基准。已登记的尝试保留当时额度；重启按账本恢复计时和现金。实际投入受盘口深度、成交条件及可用模拟现金约束；余额不足时不下单。报告单列这类观察次数。
- 本轨道只运行模拟账本，不从 008 回填历史，也不以历史亏损启动加码。加码时段以 UTC 表示。

本地从仓库根目录启动：

```bash
cargo build --release --locked --bin late_resolution_winner_trading
bash scripts/run_late_resolution_winner_price_cap.sh loss-boost server-loss-boost-001
```

另开窗格查看：

```bash
bash scripts/run_late_resolution_winner_price_cap.sh loss-boost-tui server-loss-boost-001 --lang zh
bash scripts/run_late_resolution_winner_price_cap.sh loss-boost-report server-loss-boost-001
```

服务器为隔离本次发布，沿用上述脚本内容但保存为 `scripts/run_late_resolution_winner_loss_boost.sh`，使用独立的二进制路径 `target/release/late_resolution_winner_loss_boost`。服务器启动命令需加 `LATE_WINNER_BINARY=./target/release/late_resolution_winner_loss_boost`。服务器的现有 008 脚本和二进制可以继续使用。

首次创建批次时可通过 `LATE_RESOLUTION_WINNER_LOSS_BOOST_BASE_USDC` 和 `LATE_RESOLUTION_WINNER_LOSS_BOOST_CAPITAL_USDC` 设置基准额度和模拟本金。设置会登记在账本中；改变它们需要新批次，不覆盖已有账本。额度范围 1.00–100.00 USDC，模拟本金须至少覆盖一笔 10 倍预算。

验证报告按资产和基准/加码阶段分别列出成交、未成交、净盈亏、资金占用和触发记录。应使用 008 与新轨道共同运行的时段做对照；两进程独立读取盘口，同市场未必得到完全相同的模拟成交。
