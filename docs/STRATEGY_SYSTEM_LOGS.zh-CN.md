# 分策略系统日志

独立策略使用 `<策略前缀>_SYSTEM_LOG_FILE`，polypulse 集成运行使用公共 `LOG_FILE`。配置在 .env 中自动加载，不再需要在命令前加 LOG_FILE。

| 独立程序 | 环境变量 |
|---|---|
| binary_complement_arbitrage | BINARY_COMPLEMENT_ARBITRAGE_SYSTEM_LOG_FILE |
| late_resolution_winner_trading | LATE_RESOLUTION_WINNER_TRADING_SYSTEM_LOG_FILE |
| wallet_copy_trading | WALLET_COPY_TRADING_SYSTEM_LOG_FILE |
| market_making | MARKET_MAKING_SYSTEM_LOG_FILE |
| cross_market_probability_arbitrage | CROSS_MARKET_PROBABILITY_ARBITRAGE_SYSTEM_LOG_FILE |
| bayesian_consistency_arbitrage | BAYESIAN_CONSISTENCY_ARBITRAGE_SYSTEM_LOG_FILE |
| neg_risk_conversion_arbitrage | NEG_RISK_CONVERSION_ARBITRAGE_SYSTEM_LOG_FILE |
| cross_venue_arbitrage | CROSS_VENUE_ARBITRAGE_SYSTEM_LOG_FILE |
| short_duration_digital_fair_value | SHORT_DURATION_DIGITAL_FAIR_VALUE_SYSTEM_LOG_FILE |

新系统日志保存 tracing 事件，沿用日志轮转设置。策略专用 journal、结果、报告不改路径；程序直接 println 的报告/进度内容仍在控制台，不等同于 tracing 系统日志。Market Making 等研究程序新增文件 tracing 输出，同时保留控制台 tracing。

本地 .env 的公共 LOG_FILE 已设为 logs/polypulse/system.log，避免集成进程与独立 Binary 进程共用系统日志文件。

Binary 和 Winner 保留专用策略日志，系统日志不重复专用策略事件。Wallet standalone 的系统日志包含其进程 tracing，旧 WALLET_COPY_TRADING_LOG_FILE 仍用于集成模式策略日志；新变量缺省时保留旧路径回退。Digital Fair Value 新变量缺省时兼容原 SHORT_DURATION_DIGITAL_FAIR_VALUE_LOG_FILE。

其余独立程序新变量缺省或为空时使用各自 logs/<strategy>/system.log 默认路径，避免自动混入公共 LOG_FILE。新变量优先于公共 LOG_FILE；临时覆盖也应使用新变量。同一策略的 5m/15m 或多台服务器批次仍需要不同路径。

当前 Winner 的 .env 设置：

```dotenv
LATE_RESOLUTION_WINNER_TRADING_SYSTEM_LOG_FILE=logs/server-low-latency-004-5m/system.log
```

更新二进制和 .env 后直接运行：

```bash
./late_resolution_winner_trading dry-run
```

本页替代旧隔离修复说明中用公共 LOG_FILE 覆盖 Winner 的启动方式。新批次同步调整系统日志、策略日志、订单和评估路径；普通重启追加同批次。历史文件不迁移。
