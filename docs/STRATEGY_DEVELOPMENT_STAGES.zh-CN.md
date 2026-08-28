# 策略三阶段开发、调试与准入流程

本项目将“研究可行”“真实行情可运行”和“允许真实账户写入”分成三个独立阶段。
阶段不能因为程序能够编译而自动晋级。

## 可执行文件与起始阶段

| 策略 | 独立可执行文件 | 第一阶段 PolyBacktest | 第二阶段真实数据 dry-run | 第三阶段 TUI 实盘 |
|---|---|---:|---:|---:|
| Spread Arbitrage | `spread_arbitrage` | 支持 | 支持 | `polypulse`，通过准入后 |
| Winner Buy | `winner_buy` | 支持，但受 TWAP/费用字段限制 | 支持 | `polypulse`，通过准入后 |
| Copy Trading | `copy_trader` | 不支持：数据没有钱包逐笔交易、持仓与跟随时延 | 支持 | 当前仍禁用，等待账户级共享资金/订单归属账本 |
| Maker Liquidity | `maker_liquidity` | 不支持：数据没有历史奖励参数、队列位置及可靠逐笔成交 | `scan` / `shadow` | 当前为只读研究策略，没有真实下单器 |
| Bayesian Arbitrage | `bayesian_arb` | 不支持：BTC 5m 数据没有父子市场关系图和同步跨市场盘口 | `scan` / `shadow` | 当前为只读研究策略，没有真实下单器 |

“不支持第一阶段”不代表策略无效，只表示用当前 PolyBacktest 数据回放会产生错误的
精确感。这些策略直接从第二阶段开始，以真实公开行情做无账户写入的 shadow/PAPER 验证。

## 编译

每个策略均可单独编译，不需要编译其他策略 binary：

```bash
cargo build --release --bin spread_arbitrage
cargo build --release --bin winner_buy
cargo build --release --bin copy_trader
cargo build --release --bin maker_liquidity
cargo build --release --bin bayesian_arb

# 数据工具和最终 TUI 分开编译
cargo build --release --bin polybacktest_downloader
cargo build --release --bin polypulse
```

## 第一阶段：本地历史回放

先更新数据，再回放：

```bash
./target/release/polybacktest_downloader sync
./target/release/spread_arbitrage backtest
./target/release/winner_buy backtest
```

回放只以只读方式打开 SQLite，不初始化网络客户端、签名器或账户。报告使用时间戳文件名：

```text
results/backtests/spread-arbitrage/run-YYYYMMDDTHHMMSSZ-<uuid>.json
results/backtests/winner-buy/run-YYYYMMDDTHHMMSSZ-<uuid>.json
```

报告同时包含候选、模型准入、模拟成交、胜负、毛收益、费用、净收益以及按原因聚合的
`rejections` 漏斗，便于不同参数轮次做回归对比。

Spread 回放规则：在发现价差后等待 `BACKTEST_EXECUTION_DELAY_MS`，使用第一条达到延迟
的快照，按完整 ask 深度、流动性折扣、两腿费用和最小净利润撮合；同一五分钟市场最多
建立一个非重叠仓位。

Winner 回放规则：只接纳 `coin_twap_source=chainlink_twap`，复用生产模型的开盘参考、
数据新鲜度、波动率、反转边界、spread、深度和净 EV 检查；必须由不同的 TWAP
observation 连续确认，再在延迟后的完整盘口撮合，并用市场真实 winner 结算。

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

```bash
./target/release/spread_arbitrage dry-run
./target/release/winner_buy dry-run
./target/release/copy_trader
./target/release/maker_liquidity shadow
./target/release/bayesian_arb shadow
```

`spread_arbitrage` 和 `winner_buy` 的独立入口会在进程内强制：

```text
DRY_RUN=true
CANARY_MODE=false
TUI_ENABLED=false
关闭其他策略、Merge 和 Wind-down
```

即使服务器 `.env` 写成 live，独立入口也不能下真实订单。`copy_trader` 若发现
`COPY_DRY_RUN=false` 会拒绝启动。Maker 和 Bayesian binary 本身不包含签名和订单写入。

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
