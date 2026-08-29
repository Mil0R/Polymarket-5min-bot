# 策略正式名称与兼容约定

项目中的策略名称采用能够直接说明收益来源和风险类型的行业术语。旧名称仅作为迁移
别名保留，不再用于新代码模块、可执行文件、TUI 标题或新建结果目录。

| 正式英文名称 | 中文名称 | 旧名称 | 策略性质 | 核心逻辑 |
|---|---|---|---|---|
| Binary Complement Arbitrage | 二元互补套利 | Spread Arbitrage | 双边结构套利 | 同时买入二元市场互补结果，在含费用和滑点的总成本低于结算价值 1 时建立配对仓位 |
| Late-Resolution Winner Trading | 临近结算胜方交易 | Winner Buy | 有方向风险的临近到期交易 | 临近结算时结合参考价格、TWAP、反转风险、盘口和净期望，买入模型判定的优势结果 |
| Wallet-Copy Trading | 链上钱包跟随交易 | Copy Trading | 信号跟随 | 筛选具备持续质量的钱包，将其链上行为转换为受共识、市场质量和风险额度约束的跟随订单 |
| Market Making | 做市与流动性提供 | Maker Liquidity | 双边报价研究 | 评估 post-only 双边报价、队列成交代理、逆向选择、价差收益和 maker 激励 |
| Cross-Market Probability Arbitrage | 跨市场概率套利 | Bayesian Arbitrage | 跨市场一致性套利研究 | 寻找父市场、条件子市场或相关事件之间违反条件概率一致性的可对冲价格关系 |

## 代码和运行入口

| 正式名称 | Rust 模块 | 独立可执行文件 | 新结果目录 |
|---|---|---|---|
| Binary Complement Arbitrage | 主运行时中的互补订单簿检测模块 | `binary_complement_arbitrage` | `results/binary-complement-arbitrage/` |
| Late-Resolution Winner Trading | `late_resolution_winner_trading` | `late_resolution_winner_trading`、`late_resolution_winner_report` | `results/late-resolution-winner-trading/` |
| Wallet-Copy Trading | `wallet_copy_trading` | `wallet_copy_trading`、`wallet_copy_trading_sim`、`wallet_copy_scout` | `results/wallet-copy-trading/`、`results/wallet-copy-candidates/` |
| Market Making | `market_making` | `market_making` | `results/market-making/` |
| Cross-Market Probability Arbitrage | `cross_market_probability_arbitrage` | `cross_market_probability_arbitrage` | `results/cross-market-probability-arbitrage/` |

## 兼容与迁移

- 现有环境变量键继续有效，例如 `ARBITRAGE_*`、`WINNER_BUY_*`、
  `COPY_TRADING_*`、`MAKER_*` 和 `BAYESIAN_*`。这些键属于部署接口，暂不进行破坏性改名。
- `.env.example` 和程序默认值已指向新的日志及结果目录；本地 `.env` 的路径值也应使用
  新目录。
- 历史 `results/spread-arbitrage/`、`results/winner-buy/`、
  `results/copy-trading/`、`results/maker-liquidity/` 和
  `results/bayesian-arbitrage/` 数据保持原位，避免破坏已有分析和审计链。
- 旧独立可执行文件名不再生成。部署脚本应改用上表中的新文件名；`polypulse` 主入口
  保持不变。
