# Market Making Shadow 研究模块

该模块用于回答“当前 Polymarket 市场是否值得做 maker”这一研究问题。它只访问公开的 CLOB、Gamma 和 Data API，不读取私钥，也没有签名、下单、撤单代码。

## 已实现范围

- 市场筛选器：从当前有 Liquidity Rewards 的市场开始，按每日奖励、最低报价份额、流动性、价差、价格区间、活跃度和竞争强度排序。
- 默认优先选择 24 小时成交量至少 `$1,000`、流动性至少 `$300`、观察价差不超过 6%、费率不超过 5% 的市场；可通过 `.env` 调整。
- Shadow Maker：同时构造 YES/NO post-only bid，限制在奖励计分带内，并记录假设占用资金、完整份额折价和理论 maker rebate。
- 报价距离会根据当前两腿盘口价差动态增加保护边际；短窗口内公平价移动超过阈值时暂停报价，避免主动承接快速逆向行情。
- 按 `MARKET_MAKING_QUOTE_HALF_SPREAD_GRID` 轮换实验报价距离，报告会分别汇总各实验组，不再需要启动三个进程。
- 运行时要求最近一小时有足够成交且盘口在观察窗口内持续更新；静态盘口只记录 `quote_paused`，不会生成虚假可行报价。
- 维护 queue-aware 虚拟库存；库存偏离时自动偏移两侧报价，到达上限后只保留降低净敞口的一侧。连续负 60 秒 markout 会冷却并替换市场。
- WebSocket 提供实时盘口，过期或断线时自动回退到带指数退避的 HTTP；Data API 交易请求带短时缓存。
- 成交代理：读取公开 taker trades。相同 token 的主动 `SELL` 在 shadow bid 或更低成交，记为触价成交代理；低于 bid 的成交另记为 `trade_through` 强证据。
- Markout：在 1/5/30/60 秒等时间点计算库存 markout、按推定成交份额折算的 rebate，以及双腿均成交时的配对锁定收益。
- NDJSON 研究日志和离线汇总报告。

## 使用

编译：

```bash
cargo build --release --bin market_making
```

扫描并列出当前候选市场：

```bash
./target/release/market_making scan
```

运行 Shadow Maker：

```bash
./target/release/market_making shadow
```

按 `.env` 的 `MARKET_MAKING_RESULTS_FILE` 汇总，或指定历史文件：

```bash
./target/release/market_making report
./target/release/market_making report results/market-making/shadow-run-001/events.ndjson
```

新一轮修正后的默认结果文件为：

```bash
./target/release/market_making report results/market-making/shadow-run-003/events.ndjson
```

## Shadow v2 的保守成交口径

- 每个报价只在 `MARKET_MAKING_QUOTE_INTERVAL_SECS` 指定的生命周期内有效；生成下一报价前视为已经撤销旧报价。
- 成交代理会先扣除报价生成时同价及更优价位的可见排队量，公开成交必须穿透该队列后才计入模拟成交。
- `report` 按观察周期列出 queue-aware 成交报价数、成交资金、返佣后 markout 和资金收益率；不同观察周期不能相加。
- Liquidity Rewards 是全市场奖池，不是本策略收入。v2 会按当前可见盘口、使用同一简化评分公式给出 `visible_reward_estimate`，但它看不到隐藏/瞬时订单、账户资格和官方最终计分，故只用于候选排序，不能直接计入策略 PnL。

市场会按 `MARKET_MAKING_MARKET_REFRESH_SECS` 周期重新筛选；连续盘口/报价异常达到
`MARKET_MAKING_MAX_CONSECUTIVE_MARKET_ERRORS` 后，会按 `MARKET_MAKING_MARKET_COOLDOWN_SECS`
暂时排除，并自动寻找替代市场。`MARKET_MAKING_MIN_TIME_TO_END_SECS` 用于排除临近结束的市场。

当公平价在 `MARKET_MAKING_MIDPOINT_MOVE_WINDOW_SECS` 内移动超过
`MARKET_MAKING_MAX_MIDPOINT_MOVE`，日志会记录 `quote_paused`，该轮不会生成新报价。
`MARKET_MAKING_QUOTE_HALF_SPREAD` 是基础距离，`MARKET_MAKING_DYNAMIC_QUOTE_DISTANCE=true` 时会按当前盘口价差增加动态边际。

## Shadow v3 风控和实验

- 每次启动生成独立 `run_id`；即使多次运行追加到同一文件，`report` 也会显示每个运行是否正常 `stopped`，不会再把重启误认为一段连续运行。
- `MARKET_MAKING_QUOTE_HALF_SPREAD_GRID=0.01,0.02,0.03` 按轮次均匀切换实验距离，`shadow_quote.base_quote_half_spread` 保存实验组。
- `MARKET_MAKING_MIN_TRADES_1H` 和 `MARKET_MAKING_MIN_BOOK_UPDATES` 同时约束交易活跃度与盘口活跃度。
- 高费率市场需要满足 `MARKET_MAKING_MIN_VOLUME_24H_USDC × MARKET_MAKING_HIGH_FEE_VOLUME_MULTIPLIER` 的额外成交量门槛。
- `MARKET_MAKING_MAX_INVENTORY_SHARES` 限制净 YES/NO 虚拟库存；超过上限时转为 `yes-only` 或 `no-only` 风险缩减模式，并将理论 Liquidity Rewards 记为 0。
- `MARKET_MAKING_MAX_QUOTES_PER_MARKET_HOUR` 限制单市场每小时报价次数，避免低质量市场占用全部研究预算。
- 最长 markout 周期连续负收益达到 `MARKET_MAKING_MAX_CONSECUTIVE_NEGATIVE_MARKOUTS` 后，市场进入冷却并触发重新筛选。
- `report` 除总体结果外，还会按 run、市场分类、市场、价格区间、UTC 小时和报价距离实验组汇总。

建议至少积累 100 个 60 秒周期的 queue-aware 独立成交报价，且保守返佣后的
60 秒 markout 为正，再考虑下一阶段；在拿到实际 Liquidity Rewards 分配并校准
可见竞争份额估算前，不应进入真实下单。

## 结果解释

- v2 的 `yes/no_fill_proxy_shares` 会扣除报价时可见的前方队列并限制报价生命周期，明显保守于 v1，但仍不是真实成交确认；`trade_through=true` 的证据更强。
- `inventory_markout_usdc` 衡量被动成交后的价格选择风险，负值表示成交后价格向不利方向移动。
- `estimated_rebate_usdc` 只按市场公开费率计算理论 rebate；没有费率的市场为 0。
- Liquidity Rewards 的最终份额取决于同一计分周期内其他 maker 的分数。本模块记录自身理论分数，但不会把未知奖励分配加入 PnL。
- `markout_plus_rebate_usdc` 仍未包含最终 Liquidity Rewards、真实队列、撤单延迟、网络延迟和链上结算成本，不能直接当作可实盘收益。

建议至少连续运行 3–7 天，并分别按市场分类、时间段、价格区间和 markout horizon 分组。只有当穿价样本数量充足、30/60 秒净 markout 加 rebate 稳定为正，且所需资金占用合理时，才进入小额真实挂单验证阶段。
