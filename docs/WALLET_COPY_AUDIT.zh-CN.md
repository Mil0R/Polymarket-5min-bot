# Polymarket 跟单钱包审计器

`wallet_copy_audit` 是只读、可单独运行的钱包尽调程序。它只调用公开的
Polymarket Data/Gamma/CLOB API 和可选的 Polygon JSON-RPC，不读取交易私钥，
不修改 `.env`，也不会提交订单。

## 编译与运行

```bash
cargo build --release --bin wallet_copy_audit

./target/release/wallet_copy_audit \
  0x地址1,0x地址2,0x地址3
```

也可以显式使用 `--wallets`：

```bash
./target/release/wallet_copy_audit \
  --wallets 0x地址1,0x地址2 \
  --profile conservative \
  --lookback-days 90 \
  --sync-window-secs 15 \
  --onchain-sample 200 \
  --output results/wallet-copy-audits/audits.ndjson
```

`--rpc-url` 未提供时读取 `WALLET_COPY_TRADING_ONCHAIN_RPC_URL`。传入
`--onchain-sample 0` 可以关闭交易回执和 maker/taker 对手方抽样。

## 输出

控制台为每个地址显示：

- 输入 EOA/用户地址到实际 Polymarket `proxyWallet` 的解析结果；
- `PAPER_CANDIDATE`、`RESEARCH_ONLY_REWARD_DEPENDENT`、`REJECT`、
  `REJECT_INTEGRITY` 或 `INSUFFICIENT_DATA`；
- PnL、ROI、Profit Factor、胜率、回撤、独立已结算市场；
- 当前盘口执行适配度；
- 可观测交易量、Rewards/Maker Rebate 金额及奖励依赖率；
- Polygon 回执覆盖率、V2 `OrderFilled` 对手方集中度；
- 多个输入地址之间的共享交易哈希、同步同向/反向交易及直接成交关系；
- 明确的拒绝原因、数据缺失和方法限制。

完整结果以单次运行一行 JSON 的形式**追加**到：

```text
results/wallet-copy-audits/audits.ndjson
```

写入使用文件锁，重复运行不会覆盖已有审计记录。可以通过 `--output` 修改文件。

## 结论边界

- `PAPER_CANDIDATE` 只表示值得进入 dry run，不代表允许实盘。
- 公共历史数据没有完整的历史可执行订单簿，因此不能伪造 5/15/30 秒延迟收益；
  报告中的 `live_execution_fit` 只是近期市场当前盘口的代理指标。
- 地址共享交易哈希、同步交易或对手方集中是关联证据，不是共同控制人的证明。
- 链上审计当前抽样官方 V2 CTF Exchange 和 Neg Risk Exchange 的
  `OrderFilled` 回执，不声称覆盖所有旧合约和全部历史成交。
- 公共数据不能单独证明共同注资人或智能钱包 owner，报告会把资金关联状态标为
  `not_measured`，而不是错误地给出“无风险”。

查看全部参数：

```bash
./target/release/wallet_copy_audit --help
```
