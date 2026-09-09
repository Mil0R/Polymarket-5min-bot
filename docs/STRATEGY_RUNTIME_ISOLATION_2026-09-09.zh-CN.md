# 独立策略运行隔离修复

问题：Winner 独立 dry-run 已将 Binary 交易开关关闭，但公共盘口回调在检查开关前调用套利评估并启动 REST Shadow，持续写入 Binary 日志和结果。仅修改保存目录无法解决。

修复：

- 共享盘口缓存更新和恢复监控之后、Binary 评估之前检查运行开关。关闭时不生成 MarketTick，不记录 Binary 候选，也不调度新的 REST Shadow；同时清空短期确认状态，避免重新开启时复用关闭前信号。
- Winner 独立运行不初始化 Binary/Copy 专用文本日志，不写 Binary 启动和周期指标事件。Binary 独立运行同样不初始化 Winner/Copy 专用日志。
- Winner 独立 dry-run 的共享 Binary 风控组件采用内存订单状态，不创建、读取或修复 Binary 磁盘订单文件。该构造入口强制要求 dry-run。Winner 自身订单和评估 journal 保持持久化；集成模式和实盘恢复仍使用原磁盘状态。
- TUI 中关闭 Binary 后停止新增评估/研究任务；关闭前已经启动的任务和订单恢复不在此次改动中强行取消。

后续已增加分策略系统日志：Winner 独立程序读取 LATE_RESOLUTION_WINNER_TRADING_SYSTEM_LOG_FILE，polypulse 集成模式读取公共 LOG_FILE。详见 STRATEGY_SYSTEM_LOGS.zh-CN.md。

编译：

```bash
cargo build --release --bin late_resolution_winner_trading --bin binary_complement_arbitrage --bin polypulse
```

更新服务器二进制、停止旧 Winner 进程后启动：

```bash
./late_resolution_winner_trading dry-run
```

Winner 策略日志和结果仍使用 .env 的 004-5m 路径。该命令沿用现有批次，分析时需按 run_id 分开；若建立新测试批次，应同步修改 Winner 专用日志、订单、评估与系统日志四个路径。Binary 历史 Shadow 数据可能混有旧 Winner 进程采样，不能仅凭文件路径归因于 Binary 独立运行。

验证覆盖：关闭策略返回空 tick 且不增加 Binary 评估指标、重新开启可正常产生机会；内存状态记录与终态转换；现有磁盘 journal 和恢复回归测试。服务端长时间验收应检查 Winner 评估继续增长，Binary 专用日志/结果在没有其他 Binary 进程运行时保持不变。

本地 `cargo test --lib --quiet`：305 项测试全部通过；`git diff --check` 通过。
