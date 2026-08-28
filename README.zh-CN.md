# Polymarket-5min-bot

[English](./README.md) | **中文**

**官方网站**：[polypulse.wiki](https://polypulse.wiki/)

> 面向 [Polymarket](https://polymarket.com) 加密货币「Up or Down」5 分钟预测市场的 Rust 套利机器人。

![account](image/account.png)

![运行界面](image/main.png)

## 基本原理

Polymarket 的 Up/Down 市场每 5 分钟（UTC）开一个新窗口，每个市场有 YES 和 NO 两个结果代币。

理论上，持有等量 YES + NO 可在结算时兑换 1 USDC，因此：

```
YES 卖一价 + NO 卖一价 < 1  →  存在套利空间
```

机器人大致流程如下：

1. **发现市场** — 按配置的币种（如 btc、eth）自动查找当前 5 分钟窗口对应的 Up/Down 市场。
2. **监控订单簿** — 订阅 CLOB 订单簿，实时计算 YES + NO 的合计价格。
3. **执行套利** — 当合计价格低于阈值时，先提交最佳卖盘深度较薄的一腿；确认并冻结其真实成交后，再按成交量提交另一腿。可通过滑点、单笔上限、执行价差等参数控制下单行为。
4. **Merge 回收** — 若同时持有 YES 和 NO，可链上 Merge 合并为 USDC/pUSD，减少持仓风险。
5. **窗口收尾** — 接近窗口结束时，可自动取消挂单、Merge、并卖出剩余单腿仓位。

> 本程序连接真实市场与真实资金，使用前请充分理解风险。

## 快速开始

### 预编译可执行文件

如果你不会编译代码，请直接使用 **[Releases](../../releases/latest)** 中提供的预编译可执行文件：

1. 下载对应系统（Linux / Windows）的安装包
2. 复制 `.env.example` 为 `.env`，并填写必填项
3. 运行：
   - Linux / macOS：`./polypulse`
   - Windows：`polypulse.exe`

### 从源码编译

需要先安装 [Rust](https://rustup.rs)。

```bash
cp .env.example .env   # 填写 .env 配置文件的必填项后启动
cargo run              # 运行程序
```

只读在线稳定性测试（默认 30 分钟，可传入秒数）：

```bash
./scripts/dry_run_soak.sh 1800
```

详细参数说明见 `.env.example`，按 `[1]` ~ `[9]` 分区排列：越靠前越重要。

## 基础配置

### 必填

| 变量 | 说明 |
|------|------|
| `POLYMARKET_PRIVATE_KEY` | 签名私钥。邮箱/Magic 从 [reveal.magic.link/polymarket](https://reveal.magic.link/polymarket) 导出；浏览器钱包导出对应 EOA 私钥 |
| `POLYMARKET_PROXY_ADDRESS` | 资金托管地址（Settings 里的 Address，非 EOA），见 [polymarket.com/settings](https://polymarket.com/settings) |

### 签名类型 `SIGNATURE_TYPE`

按 **Settings 里资金钱包的类型** 选择，与「邮箱还是浏览器钱包注册」无必然对应：

| 值 | 适用场景 |
|----|----------|
| `Poly1271`（**默认**） | V2 deposit wallet；邮箱/Magic 与浏览器钱包授权账号均适用 |
| `Proxy` | 仅 V1 旧 Magic 代理（Settings 地址须等于 ProxyFactory 从 EOA 推导的地址） |
| `GnosisSafe` | Gnosis Safe 多签 |
| `Eoa` | 纯 EOA 直连，无需 `POLYMARKET_PROXY_ADDRESS` |

**判断方法：** 保持默认 `Poly1271` 即可。若误设 `Proxy` 且下单报 `please use the deposit wallet flow`，说明账号已走 V2 deposit wallet，应改回 `Poly1271`（私钥与 `POLYMARKET_PROXY_ADDRESS` 无需改动）。

### Merge 所需（启用定时 Merge 或收尾时必填）

| 变量 | 说明 |
|------|------|
| `POLY_BUILDER_API_KEY` | Builder API Key |
| `POLY_BUILDER_SECRET` | Builder API Secret |
| `POLY_BUILDER_PASSPHRASE` | Builder API Passphrase |

以上三项在 Polymarket → Settings → Builder 获取。

### 常用可选项

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `CRYPTO_SYMBOLS` | `btc,eth,sol,xrp` | 监控的币种，逗号分隔 |
| `ARBITRAGE_EXECUTION_SPREAD` | `0.01` | 执行阈值：`yes + no <= 1 - spread` 时下单 |
| `MAX_ORDER_SIZE_USDC` | `100.0` | 单腿最大名义金额；按最终限价换算 share 数量 |
| `RISK_MAX_EXPOSURE_USDC` | `1000.0` | 全账户持仓成本与待提交订单的最大风险敞口 |
| `ARBITRAGE_FEE_BUFFER` | `0.002` | 按市场参数计算动态手续费后，仍必须保留的模型/舍入安全边际 |
| `DRY_RUN` | `false` | 演练模式：使用真实只读数据并模拟成交，不执行账户写操作 |
| `CANARY_MODE` | `false` | 极小额实盘硬闸门；实盘启用时必须使用 FOK/FAK |
| `CANARY_MAX_PAIR_NOTIONAL_USDC` / `CANARY_MAX_TOTAL_NOTIONAL_USDC` | `3.0` / `3.0` | 单订单对及单进程累计名义金额上限 |
| `CANARY_MAX_LOSS_USDC` | `2.0` | 单腿残余成本等保守损失预算，触发后永久暂停本进程开仓 |
| `CANARY_MAX_PAIRS_PER_WINDOW` / `CANARY_MAX_PAIRS_TOTAL` | `1` / `1` | 每窗口及单进程订单对硬上限 |
| `ARBITRAGE_BUDGET_USDC` / `WINNER_BUY_BUDGET_USDC` / `COPY_TRADING_BUDGET_USDC` | `1000` / `50` / `10000` | 三策略独立可用额度；TUI 分别显示并在下单前硬限制 |
| `PAPER_INITIAL_BALANCE_USDC` | `50.0` | 旧版 PAPER 默认额度；未配置 Winner Buy 独立额度时作为兼容回退 |
| `PAPER_LIQUIDITY_HAIRCUT` | `0.75` | 模拟时只认为盘口显示深度的该比例可以成交 |
| `PAPER_SECOND_LEG_DELAY_MS` | `250` | 第一腿模拟后等待多久重新读取第二腿真实盘口 |
| `MAX_BOOK_AGE_MS` / `MAX_BOOK_SKEW_MS` | `2000` / `1000` | 拒绝过期或两腿时间差过大的行情快照 |
| `SIGNAL_COOLDOWN_MS` | `3000` | 相同市场和价格信号的去重冷却时间 |
| `ARBITRAGE_STATE_JOURNAL_FILE` | `results/spread-arbitrage/{paper|live}/orders.ndjson` | 套利订单对与恢复状态；旧 `STATE_JOURNAL_FILE` 仍兼容 |
| `WINNER_BUY_STATE_JOURNAL_FILE` | `results/winner-buy/{paper|live}/orders.ndjson` | Winner Buy 下单、成交与结算状态 |
| `WINNER_BUY_EVALUATION_JOURNAL_FILE` | `results/winner-buy/{paper|live}/evaluations.ndjson` | Winner Buy 候选与反事实评估数据 |
| `WINNER_BUY_MAX_BOOK_AGE_MS` | `1000` | Winner Buy 独立 WS 行情年龄；疑似候选或缺书时才进行严格 REST 确认 |
| `ARBITRAGE_SHADOW_LOG_FILE` | 未设置 | 可选的深度拒绝候选 NDJSON 研究日志，不改变准入阈值 |
| `LOG_FILE` | TUI 下为 `logs/system.log` | 系统日志；配置分策略日志后不再重复对应策略事件 |
| `ARBITRAGE_STRATEGY_LOG_FILE` / `WINNER_BUY_STRATEGY_LOG_FILE` | 未设置 | 可选、互斥的分策略文本日志 |
| `MERGE_INTERVAL_MINUTES` | `0` | 定时 Merge 间隔（分钟），`0` 为关闭 |
| `WIND_DOWN_BEFORE_WINDOW_END_MINUTES` | `0` | 窗口结束前收尾（分钟），`0` 为关闭 |
| `RUST_LOG` | `info` | 日志级别 |
| `LOG_ROTATE_MAX_BYTES` | `10485760` | 每个日志文件的运行期按大小轮转阈值 |
| `LOG_ROTATE_FILES` | `5` | 保留的编号日志备份数量 |
| `OBSERVABILITY_BIND` | 未设置 | 可选本地 `/livez`、`/readyz`、兼容 `/healthz` 与 Prometheus `/metrics` |

其余参数（CLOB 地址、签名类型、滑点、订单类型、持仓同步等）均有合理默认值，一般无需修改。完整列表与注释见 `.env.example`。

Winner Buy 应按 `market_resolved` 独立市场统计，避免重复评估放大样本。可直接生成手续费后、Shadow 成交后以及 Wilson 95% 胜率下界报告：

```bash
cargo run --release --bin winner_buy_report -- \
  results/winner-buy/server-paper-run-001/evaluations.ndjson
```

不传文件名时，命令优先读取 `WINNER_BUY_EVALUATION_JOURNAL_FILE`。

报告同时输出独立市场结果和经过连续确认、延迟撮合后的 shadow cohort；后者按 shadow 自身的 evaluation 方向结算，可用于比较 `price_096_097`、`chainlink_direction_agrees` 与 `hybrid_chainlink_veto`，不会改变生产准入。

启动时会用 CLOB 累计成交和 Data API 持仓自动对账，并重建可管理的恢复任务。状态日志新记录带版本号和 SHA-256 校验和；若崩溃只损坏最后一行，启动时会将其隔离为 `.corrupt-*` 文件并保留此前有效记录。中间记录损坏仍会阻止启动。缺少上下文的旧日志默认阻止实盘启动；`ALLOW_UNRESOLVED_STATE=true` 只允许进程进入核对/恢复流程，未解决状态仍禁止新实盘开仓。

首次极小额验证建议使用 `DRY_RUN=false`、`CANARY_MODE=true`、`ARBITRAGE_ORDER_TYPE=FAK`。`/readyz` 只有在初始化、行情/用户流连接、journal 可写、磁盘空间充足、无未解决订单且实盘安全闸门未熔断时才返回 200。任何无法确认结果的 CLOB 写请求都会跨窗口暂停新开仓，必须重启并完成账户/订单核对，不能在进程内手工解除。

`DRY_RUN=true` 使用真实 CLOB 多档盘口进行纸面撮合：困难腿先读取实时订单簿并按限价逐档成交，等待 `PAPER_SECOND_LEG_DELAY_MS` 后重新读取另一腿，再按第一腿虚拟成交量模拟 FOK/FAK/即时可成交部分。它不会恢复真实 recovery、同步真实持仓、下单、撤单、Merge 或卖出。Prometheus 提供 `polypulse_paper_pairs_total`、`polypulse_paper_one_sided_total` 和 `polypulse_paper_net_pnl_usd`。

## 跟单候选钱包发现

独立的只读 `copy_wallet_scout` 可按市场一级/二级分类、近期活跃度、已结算样本、收益质量、风险和当前盘口适配度筛选跟单候选地址。它不读取密钥、不下单，也不直接修改 `.env`。编译、参数和输出说明见 [跟单候选钱包发现器](docs/COPY_WALLET_SCOUT.zh-CN.md)。

PAPER 模式还支持运行质量审计和候选池热替换：不合格钱包先进入 `Probation`，再从 scout 原子更新的 `report.json` 中选择同领域、运行时漏斗复核通过的替代钱包；LIVE 模式不会自动轮换。

## 免责声明

本软件仅供学习与研究，不构成任何投资建议。加密货币与预测市场存在较高风险，可能导致资金损失。使用前请自行评估风险，并遵守 Polymarket 服务条款及当地法律法规。
