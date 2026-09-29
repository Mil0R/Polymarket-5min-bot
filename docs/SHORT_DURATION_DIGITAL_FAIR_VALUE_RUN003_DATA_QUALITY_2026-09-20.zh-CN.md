# Short Duration Digital Fair Value：003 数据质量修正

`research-model-audit-002` 约 13 小时运行中，`crypto_prices`、`crypto_prices_chainlink` 和 `crypto_prices_twap_sixty` 都持续覆盖 BTC/ETH/SOL，但 Binance 价格被错误缩小了约 `10¹⁸` 倍。根因是旧解析器对所有流的 `full_accuracy_value` 一律按 Chainlink E18 原始值处理。Polymarket [RTDS 文档](https://docs.polymarket.com/market-data/realtime-data)中的 Binance `value` 是人类可读的美元价格。本版优先使用 Binance `value`，仅在缺失时将其 `full_accuracy_value` 当作已标度的十进制美元值；Chainlink 现货与 60 秒 TWAP 保持原有的 E18 精度解析。

002 还出现 16 次三种参考流同步空档，单次约 34–39 秒。旧连接健康检查把 `PONG` 等非价格帧也视为新数据，因此没有报告价格流停顿；此外，共享 RTDS 模块的消息被服务器 `.env` 的另一策略日志分流配置从 SDDFV 系统日志中筛掉。本版单独跟踪有效且时间戳推进的 TWAP 更新：研究模式达到 12 秒阈值后在下一次 5 秒心跳检查时主动重连，并将研究模式的连接/重连消息写入 SDDFV 系统日志；每种流/资产本地接收间隔达到 10 秒时，在事件文件写入 `reference_feed_gap`，报告汇总按流计数及最大间隔。重连只能缩短可恢复的空档，不能保证服务端或网络持续供数。

旧 001/002 原始结果保留不变；脚本拒绝向它们追加新 dry-run 事件。新批次在服务器项目根目录运行：

旧批次没有 `reference_feed_gap` 遥测，重新生成旧报告时该统计显示为 `null`（未知），不能解读成零次空档。

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_model_audit.sh dry-run research-model-audit-003
```

新数据写到 `logs/short-duration-digital-fair-value/research-model-audit-003-5m-30s/system.log` 和 `results/short-duration-digital-fair-value/research-model-audit-003-5m-30s/events.ndjson`。本轮 `.env` 无需修改；更新 release 二进制与 `run_digital_fair_value_model_audit.sh` 即可。服务器如尚未部署，应同时保留 `run_digital_fair_value_research.sh` 及 `results/short-duration-digital-fair-value/study-006-007-v2/model.json`。

先跑 30–60 分钟验收：三个资产的 Binance 价格应回到合理美元量级，与同期 Chainlink 现货同量级；三种流均有观测；查看 `reference_feed_gap` 与系统日志中的 `twap_updates_stale`，确认空档频率及重连恢复情况。通过后再继续完整 24 小时。现货流仍只供研究，不替代结算 TWAP，也不改变 dry-run 交易决策。
