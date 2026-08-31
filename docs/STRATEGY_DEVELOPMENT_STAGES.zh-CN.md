# 策略三阶段开发、调试与准入流程

本项目将“研究可行”“真实行情可运行”和“允许真实账户写入”分成三个独立阶段。
阶段不能因为程序能够编译而自动晋级。

## 可执行文件与起始阶段

| 策略 | 独立可执行文件 | 第一阶段 PolyBacktest | 第二阶段真实数据 dry-run | 第三阶段 TUI 实盘 |
|---|---|---:|---:|---:|
| Binary Complement Arbitrage | `binary_complement_arbitrage` | 支持 | 支持 | `polypulse`，通过准入后 |
| Late-Resolution Winner Trading | `late_resolution_winner_trading` | 支持，但受 TWAP/费用字段限制 | 支持 | `polypulse`，通过准入后 |
| Short-Duration Digital Fair Value | `short_duration_digital_fair_value` | 支持方向、概率校准和 Taker 成本情景回测 | 支持；强制只读且锁定已通过的 10 秒检查点 | 尚未接入 |
| Wallet-Copy Trading | `wallet_copy_trading` | 不支持：数据没有钱包逐笔交易、持仓与跟随时延 | 支持 | 当前仍禁用，等待账户级共享资金/订单归属账本 |
| Market Making | `market_making` | 不支持：数据没有历史奖励参数、队列位置及可靠逐笔成交 | `scan` / `shadow` | 当前为只读研究策略，没有真实下单器 |
| Cross-Market Probability Arbitrage | `cross_market_probability_arbitrage` | 不支持：BTC 5m 数据没有父子市场关系图和同步跨市场盘口 | `scan` / `shadow` | 当前为只读研究策略，没有真实下单器 |

“不支持第一阶段”不代表策略无效，只表示用当前 PolyBacktest 数据回放会产生错误的
精确感。这些策略直接从第二阶段开始，以真实公开行情做无账户写入的 shadow/PAPER 验证。

## 编译

每个策略均可单独编译，不需要编译其他策略 binary：

```bash
cargo build --release --bin binary_complement_arbitrage
cargo build --release --bin late_resolution_winner_trading
cargo build --release --bin short_duration_digital_fair_value
cargo build --release --bin wallet_copy_trading
cargo build --release --bin market_making
cargo build --release --bin cross_market_probability_arbitrage

# 数据工具和最终 TUI 分开编译
cargo build --release --bin polybacktest_downloader
cargo build --release --bin polypulse
```

## 第一阶段：本地历史回放

先更新数据，再回放。`BACKTEST_MARKET_TYPE` 是强制回放边界，Binary Complement 和
Late-Resolution Winner Trading 只读取该周期的完整市场，避免把 5m 与 15m 混合成一份收益结论：

```bash
./target/release/polybacktest_downloader sync
./target/release/binary_complement_arbitrage backtest
./target/release/late_resolution_winner_trading backtest
./target/release/short_duration_digital_fair_value backtest
```

回放只以只读方式打开 SQLite，不初始化网络客户端、签名器或账户。两个策略每轮都使用独立目录：

```text
results/backtests/binary-complement-arbitrage/<5m|15m>/run-YYYYMMDDTHHMMSSZ-<uuid>/
├── run.log
├── report.json
└── analysis.zh-CN.md
results/backtests/late-resolution-winner-trading/<5m|15m>/run-YYYYMMDDTHHMMSSZ-<uuid>/
├── run.log
├── report.json
└── analysis.zh-CN.md
results/backtests/short-duration-digital-fair-value/run-YYYYMMDDTHHMMSSZ-<uuid>/
├── run.log
├── report.json
└── analysis.zh-CN.md
```

Short-Duration Digital Fair Value 在多个剩余时间检查点，以窗口开始附近的首个
Chainlink TWAP observation 为 Price to Beat，并通过最终 TWAP 与 confirmed winner
审计该映射；模型使用当时之前的 TWAP 变化估计短期波动率，输出方向准确率、
Brier Score、Log Loss，并与同一时点的市场隐含概率比较。成本层使用延迟后的完整 L2
逐档成交、动态费用假设和流动性折扣；每个检查点/执行情景在一个市场最多产生一笔仓位。
准入仅使用按时间排序最后 20% 的样本外数据；成本检查点必须先在验证集为正，再在
测试集为正，不能事后从测试集挑选盈利窗口。只有方向质量和至少一个最保守成本情景同时达到门槛，报告才会给出
`READY_FOR_DRY_RUN_REVIEW`。独立 `dry-run` 使用公开 Gamma、RTDS 和 CLOB 数据，不构造
私钥、API 凭据、签名器或交易客户端；当前只允许全量回测通过的剩余 10 秒检查点，其他
检查点 fail-closed。

报告同时包含候选、模型准入、模拟成交、胜负、毛收益、费用、净收益以及按原因聚合的
`rejections` 漏斗，便于不同参数轮次做回归对比。

Binary Complement Arbitrage 回放规则：先要求价差和可信 ask 深度连续存在到场景确认时间，再经过场景化
预检延迟用同一快照检查双腿及第二腿深度覆盖倍数，并先成交信号中较薄的一腿。第二腿按 FOK 语义在后续
快照撮合；失败后按场景延迟读取第一腿 bid 并尝试完整紧急卖回，无法退出才按真实 winner 结算方向性风险。
完整撮合还应用流动性折扣、信号限价、双腿最低 $1、fee buffer 和最小净利润。同一市场周期内每个市场、每个场景
最多建立一个非重叠仓位。

Late-Resolution Winner Trading 回放规则：只接纳 `coin_twap_source=chainlink_twap`，复用生产模型的开盘参考、
数据新鲜度、波动率、反转边界、spread、深度和净 EV 检查；必须由不同的 TWAP
observation 连续确认，再在延迟后的完整盘口撮合，并用市场真实 winner 结算。

Binary Complement 使用“生产基准 + 单变量敏感性 + 两个组合压力场景”，避免高维笛卡尔组合造成过拟合；
由于 Polybacktest 是离散快照而不是逐条 CLOB 事件，回测额外保留一组 `preflight=0` 的快速研究场景，
用于分别观察确认时间、第二腿延迟、覆盖倍数和退出延迟。它只代表“同一快照内可成交”的乐观上界；
任意正预检延迟可能直接落到下一张快照，是保守下界，不能据此推断真实 25–250ms REST 延迟表现。
Late-Resolution Winner Trading 分别使用 BTC、ETH、SOL/其他资产的 `MIN_VOLATILITY_BPS` 和
`MODEL_RISK_BUFFER`：

```dotenv
BACKTEST_MARKET_TYPE=5m
BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_ORDER_SIZES_USDC=2.5,5,10
BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_CONFIRMATION_MS=100,250,500,1000
BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_PREFLIGHT_DELAYS_MS=0,50,100,250
BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_SECOND_LEG_DELAYS_MS=0,25,50,100,250,500
BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_LIQUIDITY_HAIRCUTS=0.50,0.75,1.0
BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_SECOND_LEG_DEPTH_MULTIPLIERS=1,2,3,5
BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_EMERGENCY_UNWIND_DELAYS_MS=0,25,50,100
BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_MAX_SLIPPAGE_PER_SHARE=0.01
# 紧急退出后最终未恢复单腿率上限
BINARY_COMPLEMENT_ARBITRAGE_BACKTEST_MAX_ONE_SIDED_RATE=0.01
LATE_RESOLUTION_WINNER_TRADING_BACKTEST_DELAYS_MS=100,250,500,1000
LATE_RESOLUTION_WINNER_TRADING_BACKTEST_LIQUIDITY_HAIRCUTS=0.50,0.75,1.0
SHORT_DURATION_DIGITAL_FAIR_VALUE_CHECKPOINTS_SECS=240,180,120,60,30,10
SHORT_DURATION_DIGITAL_FAIR_VALUE_BACKTEST_DELAYS_MS=100,250,500,1000
SHORT_DURATION_DIGITAL_FAIR_VALUE_BACKTEST_LIQUIDITY_HAIRCUTS=0.50,0.75,1.0
SHORT_DURATION_DIGITAL_FAIR_VALUE_BACKTEST_FEE_RATE=0.07
SHORT_DURATION_DIGITAL_FAIR_VALUE_BACKTEST_FEE_EXPONENT=1
BACKTEST_PROGRESS_EVERY_MARKETS=100
BACKTEST_MARKET_OFFSET=0
BACKTEST_MAX_MARKETS=0
```

### 独立验证 15 分钟市场

不要把 15m 追加进现有 5m 回测文件后再依赖默认配置。推荐使用独立数据库：

```bash
POLYBACKTEST_MARKET_TYPE=15m \
POLYBACKTEST_DATABASE_FILE=data/polybacktest/polybacktest-15m.sqlite3 \
./target/release/polybacktest_downloader sync

BACKTEST_MARKET_TYPE=15m \
BACKTEST_DATABASE_FILE=data/polybacktest/polybacktest-15m.sqlite3 \
./target/release/binary_complement_arbitrage backtest

BACKTEST_MARKET_TYPE=15m \
BACKTEST_DATABASE_FILE=data/polybacktest/polybacktest-15m.sqlite3 \
./target/release/late_resolution_winner_trading backtest
```

回测报告中的 `market_type`、启动日志和输出目录会共同记录周期。若数据库没有该周期的
完整市场，程序直接报错，不会回退到另一周期。15m 回测达到准入门槛后，再设置
`MARKET_PERIOD_MINUTES=15` 运行对应独立 `dry-run`；5m 与 15m 不应在同一进程中同时验证。

程序只读取每个市场快照一次，并同时评估所有场景。`report.json` 保存总体和分资产漏斗、成交、胜负、费用与 PnL；`analysis.zh-CN.md` 自动比较场景稳定性并列出主要拒绝原因；`run.log` 保存启动、进度、耗时和产物路径。可设置 `BACKTEST_MARKET_OFFSET` 和 `BACKTEST_MAX_MARKETS=100` 做指定区段的 smoke test，正式分析必须把两者恢复为 `0`。

当前数据没有历史逐市场 fee rate/exponent，因此 `BACKTEST_FEE_RATE` 和
`BACKTEST_FEE_EXPONENT` 是明确的保守假设。报告会保留这一限制，不能把回放结果直接
等同于可实现收益。

第一阶段建议至少完成：

1. 时间顺序检查，确认没有使用信号之后才知道的 winner/终局价格生成信号；
2. 对订单金额、延迟、深度折扣、费用和阈值做敏感性网格；
3. 按市场而不是按快照计算样本量，避免高频快照伪造显著性；
4. 保留每次时间戳报告，不覆盖上一轮参数结果；
5. 只有在保守成本下仍有稳定样本，才进入服务器 dry-run。

## 第二阶段：服务器真实行情 dry-run

Short-Duration Digital Fair Value 独立运行：

```bash
./target/release/short_duration_digital_fair_value dry-run
```

它只在剩余 10 秒内比较模型概率与可成交 ask，候选通过后等待配置的 1000ms，再读取公开
REST 完整盘口，按 50% 深度折扣逐档模拟成交。控制台和日志仅记录启动、机会、Shadow
成交、结算及异常；所有机会、延迟后拒绝、成交和最终 0/1 PnL 追加写入
`results/short-duration-digital-fair-value/paper/events.ndjson`。如果进程在等待结算期间退出，
已写入的 Shadow 成交不会丢失，但当前版本不会在重启后自动恢复其在线结算任务，事后分析
应按 slug 补查 winner。

Binary Complement 的 PAPER 执行在第二腿全部或部分失败时，会等待配置的退出延迟，重新读取第一腿 bid，
并按 FOK 尝试卖回未配对 shares。完整回卖会释放 `unmatched_cost`，同时累计已实现的
`emergency_unwind_pnl`；盘口不足时仍保留单腿敞口，不能把紧急退出假设成必然成交。

```dotenv
BINARY_COMPLEMENT_ARBITRAGE_ORDER_TYPE=FOK
BINARY_COMPLEMENT_ARBITRAGE_PAPER_SECOND_LEG_DELAY_MS=250
BINARY_COMPLEMENT_ARBITRAGE_PAPER_EMERGENCY_UNWIND_DELAY_MS=50
BINARY_COMPLEMENT_ARBITRAGE_PAPER_EMERGENCY_UNWIND_MIN_PRICE=0.01
BINARY_COMPLEMENT_ARBITRAGE_PAPER_SUMMARY_INTERVAL_SECS=60
LATE_RESOLUTION_WINNER_TRADING_PAPER_EXECUTION_DELAY_MS=250
LATE_RESOLUTION_WINNER_TRADING_PAPER_SUMMARY_INTERVAL_SECS=60
```

纯文本 dry-run 会输出四类关键事件：可执行机会（盘口、深度、费用、预期收益、行情年龄）、双腿预检与
第二腿 REST 延迟、单次成交的配对/紧急退出/已实现 PnL，以及按上述间隔输出的累计漏斗、失败率、
REST 平均/最大延迟和未恢复敞口汇总。`realized_trade_pnl` 不包含仍未退出仓位的方向性终值；这部分会以
`unrecovered_cost` 和汇总中的 `unmatched_cost` 单独显示，避免把未结算风险误报为已实现亏损或收益。

Late-Resolution Winner Trading 的 PAPER 日志会在每次模拟成交时输出 fee-inclusive 的单笔预期收益、
实际 REST 请求延迟和包含配置等待时间的总执行延迟；市场终局后输出该仓位实际收益和进程累计实际收益。
独立周期汇总包含扫描/信号/确认/尝试/成交/结算漏斗、成交率、胜率、盘口 fallback REST 与执行 REST
平均/最大延迟、待结算成本、预期收益和实际收益。

```bash
./target/release/binary_complement_arbitrage dry-run
./target/release/late_resolution_winner_trading dry-run
./target/release/wallet_copy_trading
./target/release/market_making shadow
./target/release/cross_market_probability_arbitrage shadow
```

`binary_complement_arbitrage` 和 `late_resolution_winner_trading` 的独立入口会在进程内强制：

```text
DRY_RUN=true
CANARY_MODE=false
TUI_ENABLED=false
关闭其他策略、Merge 和 Wind-down
```

即使服务器 `.env` 写成 live，独立入口也不能下真实订单。`wallet_copy_trading` 若发现
`WALLET_COPY_TRADING_DRY_RUN=false` 会拒绝启动。Market Making 和 Cross-Market Probability Arbitrage
binary 本身不包含签名和订单写入。

每轮调整应使用新的日志/结果边界，至少覆盖正常窗口、重连、API 限速、异常退出恢复、
跨日和终局结算。建议先运行 24 小时定位漏斗，再运行 7–14 天验证参数稳定性。每轮只把
能够由数据解释的修改带入下一轮，保留旧结果用于回归对照。

## 第三阶段：只允许交互式 TUI 真实运行

第三阶段唯一入口：

```bash
./target/release/polypulse
```

当 `DRY_RUN=false` 或启用 canary 时，程序要求 `TUI_ENABLED=true` 且 stdout 是真实终端。
从 systemd、nohup、管道、重定向或任一独立策略 binary 启动真实模式都会失败关闭。

进入第三阶段前至少完成：

1. 多轮 dry-run 中信号、尝试、模拟成交、结算和净收益能够闭环对账；
2. 费用、滑点、延迟和深度折扣后的结果仍为正，不依赖单一异常市场；
3. 重启恢复、断线、过期盘口、部分成交和不确定写入测试通过；
4. 参数已经冻结并记录对应 dry-run 的 `run_id`、日志、结果与分析结论；
5. 先使用 TUI canary 硬限额，人工观察后才能扩大额度；
6. 尚未具备共享账户风险账本或真实执行器的策略继续停留在第二阶段。

第三阶段是部署准入，不是开发调试入口。参数调试继续回到第一或第二阶段完成。
