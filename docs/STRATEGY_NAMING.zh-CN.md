# 策略正式名称与兼容约定

项目中的策略名称采用能够直接说明收益来源和风险类型的行业术语。旧名称仅用于识别
历史资料，不再作为代码模块、可执行文件、TUI 标题、结果目录或环境变量接口。

| 正式英文名称 | 中文名称 | 旧名称 | 策略性质 | 核心逻辑 |
|---|---|---|---|---|
| Binary Complement Arbitrage | 二元互补套利 | Spread Arbitrage | 双边结构套利 | 同时买入二元市场互补结果，在含费用和滑点的总成本低于结算价值 1 时建立配对仓位 |
| Late-Resolution Winner Trading | 临近结算胜方交易 | Winner Buy | 有方向风险的临近到期交易 | 临近结算时结合参考价格、TWAP、反转风险、盘口和净期望，买入模型判定的优势结果 |
| Wallet-Copy Trading | 链上钱包跟随交易 | Copy Trading | 信号跟随 | 筛选具备持续质量的钱包，将其链上行为转换为受共识、市场质量和风险额度约束的跟随订单 |
| Market Making | 做市与流动性提供 | Maker Liquidity | 双边报价研究 | 评估 post-only 双边报价、队列成交代理、逆向选择、价差收益和 maker 激励 |
| Cross-Market Probability Arbitrage | 跨市场概率套利 | Bayesian Arbitrage | 跨市场一致性套利研究 | 寻找父市场、条件子市场或相关事件之间违反条件概率一致性的可对冲价格关系 |
| Short-Duration Digital Fair Value | 短期限数字期权公允价值 | 无 | 有方向风险的概率相对价值研究 | 根据 Price to Beat、Chainlink TWAP、剩余期限和实时波动率估计 Up/Down 公允概率，交易扣除成本后的盘口偏差 |

## 代码和运行入口

| 正式名称 | Rust 模块 | 独立可执行文件 | 新结果目录 |
|---|---|---|---|
| Binary Complement Arbitrage | 主运行时中的互补订单簿检测模块 | `binary_complement_arbitrage` | `results/binary-complement-arbitrage/` |
| Late-Resolution Winner Trading | `late_resolution_winner_trading` | `late_resolution_winner_trading`、`late_resolution_winner_report` | `results/late-resolution-winner-trading/` |
| Wallet-Copy Trading | `wallet_copy_trading` | `wallet_copy_trading`、`wallet_copy_trading_sim`、`wallet_copy_scout`、`wallet_copy_audit` | `results/wallet-copy-trading/`、`results/wallet-copy-candidates/`、`results/wallet-copy-audits/` |
| Market Making | `market_making` | `market_making` | `results/market-making/` |
| Cross-Market Probability Arbitrage | `cross_market_probability_arbitrage` | `cross_market_probability_arbitrage` | `results/cross-market-probability-arbitrage/` |
| Short-Duration Digital Fair Value | `digital_fair_value` | `short_duration_digital_fair_value` | `results/backtests/short-duration-digital-fair-value/`；实时目录待第二阶段实现 |

## 环境变量前缀

| 正式名称 | 新环境变量前缀 | 旧前缀（不再读取） |
|---|---|---|
| Binary Complement Arbitrage | `BINARY_COMPLEMENT_ARBITRAGE_*` | `ARBITRAGE_*` 以及无前缀的套利参数 |
| Late-Resolution Winner Trading | `LATE_RESOLUTION_WINNER_TRADING_*` | `WINNER_BUY_*` |
| Wallet-Copy Trading | `WALLET_COPY_TRADING_*` | `COPY_TRADING_*`、`COPY_*` |
| Market Making | `MARKET_MAKING_*` | `MAKER_*` |
| Cross-Market Probability Arbitrage | `CROSS_MARKET_PROBABILITY_ARBITRAGE_*` | `BAYESIAN_*` |
| Short-Duration Digital Fair Value | `SHORT_DURATION_DIGITAL_FAIR_VALUE_*` | 无 |

通用连接、签名、日志和运行模式参数（例如 `CLOB_API_URL`、`SIGNATURE_TYPE`、
`DRY_RUN`、`RUST_LOG`）继续由多个策略共享，因此不添加策略前缀。

## 部署与历史数据

- 本次是一次破坏性环境变量迁移：程序不再读取旧策略键，也不再读取旧的
  `STATE_JOURNAL_FILE` 别名。服务器必须用新的 `.env` 完整覆盖旧文件，不能只追加。
- `.env.example` 和程序默认值已指向新的日志及结果目录；本地 `.env` 的路径值也应使用
  新目录。
- 历史 `results/spread-arbitrage/`、`results/winner-buy/`、
  `results/copy-trading/`、`results/maker-liquidity/` 和
  `results/bayesian-arbitrage/` 数据保持原位，避免破坏已有分析和审计链。
- 旧独立可执行文件名不再生成。部署脚本应改用上表中的新文件名；`polypulse` 主入口
  保持不变。
