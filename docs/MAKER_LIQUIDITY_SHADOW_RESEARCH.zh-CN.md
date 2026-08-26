# Maker Liquidity Shadow 研究模块

该模块用于回答“当前 Polymarket 市场是否值得做 maker”这一研究问题。它只访问公开的 CLOB、Gamma 和 Data API，不读取私钥，也没有签名、下单、撤单代码。

## 已实现范围

- 市场筛选器：从当前有 Liquidity Rewards 的市场开始，按每日奖励、最低报价份额、流动性、价差、价格区间、活跃度和竞争强度排序。
- Shadow Maker：同时构造 YES/NO post-only bid，限制在奖励计分带内，并记录假设占用资金、完整份额折价和理论 maker rebate。
- 成交代理：读取公开 taker trades。相同 token 的主动 `SELL` 在 shadow bid 或更低成交，记为触价成交代理；低于 bid 的成交另记为 `trade_through` 强证据。
- Markout：在 1/5/30/60 秒等时间点计算库存 markout、按推定成交份额折算的 rebate，以及双腿均成交时的配对锁定收益。
- NDJSON 研究日志和离线汇总报告。

## 使用

编译：

```bash
cargo build --release --bin maker_liquidity
```

扫描并列出当前候选市场：

```bash
./target/release/maker_liquidity scan
```

运行 Shadow Maker：

```bash
./target/release/maker_liquidity shadow
```

按 `.env` 的 `MAKER_RESULTS_FILE` 汇总，或指定历史文件：

```bash
./target/release/maker_liquidity report
./target/release/maker_liquidity report results/maker-liquidity/shadow-run-001/events.ndjson
```

## 结果解释

- `yes/no_fill_proxy_shares` 是乐观代理，不是真实成交确认。同价成交无法知道前方队列；`trade_through=true` 的可信度更高。
- `inventory_markout_usdc` 衡量被动成交后的价格选择风险，负值表示成交后价格向不利方向移动。
- `estimated_rebate_usdc` 只按市场公开费率计算理论 rebate；没有费率的市场为 0。
- Liquidity Rewards 的最终份额取决于同一计分周期内其他 maker 的分数。本模块记录自身理论分数，但不会把未知奖励分配加入 PnL。
- `markout_plus_rebate_usdc` 仍未包含最终 Liquidity Rewards、真实队列、撤单延迟、网络延迟和链上结算成本，不能直接当作可实盘收益。

建议至少连续运行 3–7 天，并分别按市场分类、时间段、价格区间和 markout horizon 分组。只有当穿价样本数量充足、30/60 秒净 markout 加 rebate 稳定为正，且所需资金占用合理时，才进入小额真实挂单验证阶段。
