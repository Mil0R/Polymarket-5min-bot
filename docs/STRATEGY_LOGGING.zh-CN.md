# 分策略日志与结果文件规划

## 目标

运行数据按“策略”和“交易模式”隔离，同时保留一个系统总日志用于排查连接、认证、行情和进程级问题。订单状态文件承担崩溃恢复，不按单次启动切换文件。

## 当前目录结构

```text
logs/
├── system.log                 # 系统事件；已配置分策略日志时不重复策略事件
├── binary-complement-arbitrage.log       # Binary Complement Arbitrage 专属文本日志
├── late-resolution-winner-trading.log             # Late-Resolution Winner Trading 专属文本日志
└── wallet-copy-trading.log            # Wallet-Copy Trading 专属文本日志

results/
├── binary-complement-arbitrage/
│   ├── paper/
│   │   ├── orders.ndjson      # 模拟订单对状态与恢复生命周期
│   │   ├── opportunities.ndjson
│   │   └── shadow-candidates.ndjson # 价格有边际但深度不足的研究样本
│   └── live/
│       ├── orders.ndjson
│       └── opportunities.ndjson
└── late-resolution-winner-trading/
    ├── paper/
    │   ├── orders.ndjson      # 尝试、成交、未成交、结算及风控状态
    │   └── evaluations.ndjson # 候选、拒绝原因和反事实结果
    └── live/
        ├── orders.ndjson
        └── evaluations.ndjson
```

实际只创建当前模式和已产生数据的文件。日志与结果目录已加入 `.gitignore`。

## 文件职责

- `system.log`：连接、认证、市场发现、WebSocket 和健康检查；配置分策略日志后，对应策略事件不会在此重复写入。
- `binary-complement-arbitrage.log`：套利盘口判断、最终报价拒绝、风控拒绝、下单及恢复执行。
- `late-resolution-winner-trading.log`：TWAP、模型评估、拒绝原因、确认、下单、成交和结算。
- `wallet-copy-trading.log`：钱包画像、数据源、信号漏斗、运行质量审计、候选轮换、下单和结算。
- Binary Complement Arbitrage `orders.ndjson`：可恢复的订单对状态，是实盘安全状态，不应只作为普通统计文件处理。
- Binary Complement Arbitrage `opportunities.ndjson`：达到机会初筛门槛的数据，不代表最终可成交或已经下单。
- Binary Complement Arbitrage `shadow-candidates.ndjson`：只在 `DRY_RUN=true` 下写入研究数据，不会改变准入或触发订单。`action=depth_rejected` 记录浅深度候选；`action=admission_rejected` 记录准入拒绝原因及滑点、tick、动态手续费后的反事实净边际；`action=rest_shadow` 记录不同延迟和名义金额下的 REST 双腿可成交情况。可通过 `admission_rejection`、`passes_gross_edge`、`passes_net_edge` 区分固定门槛和真实费用影响。
- Late-Resolution Winner Trading `orders.ndjson`：订单和结算事实，也是风险熔断与重启恢复的数据源。
- Late-Resolution Winner Trading `evaluations.ndjson`：包括拒绝和未成交候选、`production` / `hybrid_paper` / `rejected_shadow` 的连续确认进度、只读研究 cohort 与延迟撮合结果，用于阈值校准并避免幸存者偏差。拒绝候选只有完成与生产一致的连续确认后才进行 shadow 撮合；启用 PAPER 混合路径时，符合 `hybrid_chainlink_veto` 的候选还会进入正常模拟订单日志。

## 配置规则

`DRY_RUN=true` 时使用 `paper` 目录，实盘和 canary 使用 `live` 目录。代码在未显式配置结果路径时会按该规则生成默认路径。

为避免状态断裂：如果旧状态文件中存在未解决订单，应先完成恢复/核对，再切换到新路径。当前 `.env` 的旧订单状态文件为空，因此已直接切换到新目录。

文本日志在运行期间持续按大小轮转，而非只在下次启动时轮转。分析某个策略时优先读取对应策略日志和结果文件；排查行情源、连接或进程问题时再结合 `system.log`。

每次进程启动会生成一个 `run_id`，并在系统日志及两个策略日志的启动记录中同时写入该 ID、paper/canary/live 模式和对应结果文件路径。由于文本日志采用追加写，分析单次运行时可用 `run_id` 启动记录作为边界。
