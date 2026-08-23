# 第十一阶段：真实数据 Paper Trading Engine

## 1. 目标

将原先 Dry-run 的“两腿必定全部成交”替换为使用真实 Polymarket 盘口、独立虚拟现金、顺序提交延迟和订单类型语义的纸面撮合，同时从代码层保证模拟模式不产生账户写操作。

## 2. 撮合流程

```text
真实 WebSocket 信号与最终报价
  → 选择最佳卖盘更薄的困难腿
  → REST 重新获取困难腿完整实时订单簿
  → 仅撮合价格不高于最终限价的多档 asks
  → 对显示深度应用 PAPER_LIQUIDITY_HAIRCUT
  → 应用 FOK 或 FAK/即时部分成交语义
  → 等待 PAPER_SECOND_LEG_DELAY_MS
  → REST 重新获取第二腿实时订单簿
  → 第二腿最多按第一腿虚拟成交量撮合
  → 记录 VWAP、配对量、单腿成本、虚拟现金与理论 PnL
```

GTC/GTD 在模拟器中只计算立即可成交部分并视为随后撤销余量，不虚构排队成交。FOK 深度不足时为零成交；FAK 深度不足时允许部分成交。

## 3. 虚拟资金

`PAPER_INITIAL_BALANCE_USDC` 默认 50 USDC。每个并发模拟任务会先原子预留计划成本，避免多个任务共同穿透余额；REST 读取失败或任务取消时由 RAII 自动释放预留。撮合完成后先扣除实际虚拟成交成本，再把完整 YES+NO 配对按每 share 1 USDC 立即计回虚拟现金；未配对腿继续按成本占用资金。

账本累计记录：

- 剩余虚拟现金；
- 总虚拟成交成本；
- 完整配对 share；
- 未配对腿成本；
- 配对部分按 `1 - yes_vwap - no_vwap` 计算的理论 PnL；
- 模拟订单对次数。

## 4. 账户写隔离

`DRY_RUN=true` 时：

- `TradingExecutor` 只调用 CLOB `order_book` 等读取接口，不 build/sign/post；
- 不执行启动 journal 恢复，不会武装真实 recovery sell；
- 不同步真实账户持仓到虚拟账本；
- 不运行 PositionBalancer 撤单；
- 不运行 scheduled Merge；
- Wind-down 不撤单、不 Merge、不卖出；
- Paper 结果不注册进真实 `RiskManager`、owned-order registry 或 state journal。

`scripts/dry_run_soak.sh` 还会强制使用 `results/<strategy>/paper/` 下的独立状态与结果文件，并关闭所有相关后台写路径，形成第二层保护。

## 5. 指标

新增：

- `polypulse_paper_pairs_total`；
- `polypulse_paper_one_sided_total`；
- `polypulse_paper_net_pnl_usd`。

每次纸面执行日志包含困难腿、两腿成交量、两腿 VWAP、配对量、剩余虚拟现金、累计理论 PnL 和累计未配对成本。

## 6. 配置建议

```env
DRY_RUN=true
CANARY_MODE=false
PAPER_INITIAL_BALANCE_USDC=50.0
PAPER_LIQUIDITY_HAIRCUT=0.75
PAPER_SECOND_LEG_DELAY_MS=250
ARBITRAGE_STATE_JOURNAL_FILE=results/spread-arbitrage/paper/orders.ndjson
WINNER_BUY_STATE_JOURNAL_FILE=results/winner-buy/paper/orders.ndjson
WINNER_BUY_EVALUATION_JOURNAL_FILE=results/winner-buy/paper/evaluations.ndjson
ARBITRAGE_LOG_FILE=results/spread-arbitrage/paper/opportunities.ndjson
POSITION_SYNC_INTERVAL_SECS=0
POSITION_BALANCE_INTERVAL_SECS=0
MERGE_INTERVAL_MINUTES=0
WIND_DOWN_BEFORE_WINDOW_END_MINUTES=0
OBSERVABILITY_BIND=127.0.0.1:9090
```

运行：

```bash
./scripts/dry_run_soak.sh 7200
```

## 7. 当前边界

1. 模拟使用两次 REST 快照，无法重建两个快照之间每一次盘口队列变化。
2. 流动性折扣是保守模型，不是个人账户在交易所撮合队列中的精确位置。
3. 当前理论 PnL 不包含最终结算延迟、链上 Merge gas、动态费率和未配对仓位之后的退出价格。
4. Paper 测试只能提高 canary 前置信心，不能替代极小额隔离账户实盘验证。

## 8. 离线验证

- 多档限价撮合测试；
- FOK 深度不足零成交；
- FAK 部分成交；
- 流动性折扣；
- 虚拟现金、理论 PnL 和未配对成本；
- 并发虚拟资金预留与自动释放；
- Paper 指标累计。

最终质量门禁：

- `cargo fmt --all -- --check`：通过；
- `cargo check --all-targets`：通过；
- `cargo clippy --all-targets -- -D warnings`：通过；
- `cargo test --all-targets`：70 个测试通过（库 7、主程序 63）；
- `bash -n scripts/dry_run_soak.sh`：通过；
- `git diff --check`：通过。
