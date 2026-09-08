# Late-Resolution Winner Trading 并行 Shadow 与参考价审计

本轮按 `server-low-latency-003-5m` 的分析新增研究能力。正式模型继续采用原参数；新组仅由 dry-run 中的只读 Shadow 执行器运行，不预留 PAPER 资金、不生成正式订单生命周期。

## 参考价核验与波动率研究

2026-09-08 查询官方 Gamma 的 `btc-updown-5m-1788828000`，描述明确指向 Chainlink `btc-usd-twap-60s-streams`，并提供 `eventMetadata.priceToBeat` 和 `finalPrice`。这支持该样本使用 60 秒 TWAP，但不代表所有资产、历史周期均已核验。公开参考：[Polymarket 同期 BTC 市场](https://polymarket.com/event/btc-updown-5m-1788807300)、[Gamma 样本](https://gamma-api.polymarket.com/events?slug=btc-updown-5m-1788828000)。

每条新模型审计记录包含 `reference_topic` 和 `horizon_volatility`：10、20、40、60 秒历史非重叠价格增量、有效增量数和 bps/√秒。跨度缺样或时间缺口过大时不填零波动率。不同候选之间仍有关联；这些统计只用于诊断 TWAP 平滑和时间相关性，不直接替代交易波动率下限。

报告可选 `--reference-audit`：按唯一 slug 查询官方 Gamma，保存 `reference-audit.json`；下次运行复用已关闭且有终值的缓存，缺失数据可重试。默认 report 只读取缓存，不发起网络请求。应避免同时运行两个带该选项的报告进程。

参考审计按 run/market/剩余时间区间选择最早评估，显示官方终值相对当时 TWAP 的误差 RMS、误差除以当前模型剩余波动后的 RMS，以及本地开盘参考与官方 priceToBeat 的最大偏差。显示缺失数据和未明确声明 60 秒来源的计数。候选样本经过筛选，时间组也重叠；不会根据这些结果自动校准参数或宣布盈利可行。

## 并行参数组

`.env` 新增：

```dotenv
LATE_RESOLUTION_WINNER_TRADING_SENSITIVITY_ENABLED=true
```

代码缺省关闭，`.env.example` 也缺省关闭；本地部署配置已开启。启用后，在 PAPER 中对每条已持久化候选运行以下固定笛卡尔组合：

| 参数 | 缩放值 |
|---|---|
| 资产现有波动率下限 | 1、0.5、0.25、0.1 |
| 资产现有模型风险缓冲 | 1、0.5 |

共 8 组，包括 `sensitivity_f1_r1` 原参数对照。实际波动率仍为实测值与该组下限的较大值；基础距离、反转 σ、深度、价差、费用和最小净 EV 保持原要求。

各组独立积累不同时间戳的连续方向确认；数据或盘口检查失败时清除确认，方向翻转和间隔过长也重新计数。每组每市场每窗口最多尝试一次。每次启动重置窗口内存状态，跨重启记录用 run_id 区分，因此不能把重启后的同市场视为新的独立统计样本。

同一评估时刻确认的组共享一次延迟盘口请求和模拟结果，以减少额外请求和组间执行差异。不同确认时刻的组在各自时刻请求。使用现有 250ms 等待、流动性折扣、订单类型和含费额度限制；保存实际 REST 及总执行耗时。组间不竞争同一笔模拟资金，收益不能相加成组合账户收益。

新增 `sensitivity_observed` 包含每组预测概率、净 EV、准入/确认状态及是否已尝试；`shadow_executed` 的 `candidate_stage=sensitivity_confirmed` 与原有 rejected Shadow 分开。正式汇总计数和原 Shadow 指标不混入研究组。控制台在研究机会确认和模拟返回时输出关键信息，结算收益由报告关联原 evaluation_id 得到。

## 报告改进

- 分别报告首次候选、首次确认、最后候选，显示方向翻转市场数与未结算数。
- 研究组按 run/market/group 的首次尝试统计，保留未成交及未结算，不用后续成功替换失败。
- 研究组与旧 Shadow 分开显示，不能相加；同一次共享成交可以出现在多个组。
- 所有收益组新增模拟成交胜率的 Wilson 95% 下界，以及成本/份数给出的近似盈亏平衡胜率。它假设可比较的获胜概率，不是精确的异质仓位风险模型。
- 旧日志仍可读取；缺失新增审计字段不会被解释成零延迟或零波动率。

## 编译与下一轮运行

```bash
cargo build --release --bin late_resolution_winner_trading --bin late_resolution_winner_report
```

将更新的 `.env` 与 `target/release/late_resolution_winner_trading` 复制到服务器运行目录：

```bash
./late_resolution_winner_trading dry-run
```

`.env` 默认 5 分钟，新路径为：

```text
logs/server-low-latency-004-5m/late-resolution-winner-trading.log
results/late-resolution-winner-trading/server-low-latency-004-5m/orders.ndjson
results/late-resolution-winner-trading/server-low-latency-004-5m/evaluations.ndjson
```

记录追加保存，新目录自动创建。正式 orders 文件仍可能为空；研究成交保存在 evaluations 中。旧 003 批次保留，分析时明确传入旧路径。

下载数据后生成新报告（本地使用 `./target/release/late_resolution_winner_report`）：

```bash
./late_resolution_winner_report results/late-resolution-winner-trading/server-low-latency-004-5m/evaluations.ndjson --reference-audit
```

报告输出到控制台，可用 `| tee report.txt` 保存。`reference-audit.json` 放在传入评估文件的同目录。

先运行约 24 小时检查组决策和记录，再延长至 72 小时观察跨日期表现。选参期和验证期应分开；8 组中历史收益最高不能直接作为上线依据。100–200 笔只适合初步方向检验，高胜率尾部损失仍需更长验证。

## 本地验证结果

- 37 项相关库测试、4 项报告测试通过；两个 release 二进制编译成功。
- 新增测试覆盖原参数研究组与原模型一致、薄盘口与过期输入不被缩放参数绕过、历史跨度估计不重叠并拒绝大缺口，以及模型缺失时不把零占位成本当作免费入场。
- 合成报告回归验证方向翻转的首次/末次选择不同、研究组不混入原 Shadow，以及同组首次亏损或未成交不被后续盈利覆盖。
- 003 批次参考价审计已实际查询并缓存官方数据。可用样本本地开盘价与官方参考价最大偏差约 0.01909bps，终点误差除以当前模型剩余波动的 RMS 约 0.112–0.414；样本有筛选偏差，尚不据此设定新的正式下限。
- 003 完整 Shadow 净收益仍为 +1.66980716 USDC。首次候选 43/44 胜，每市场买一份的含费反事实合计 -0.040315 USDC；最后候选 44/44 胜、同口径 +0.884977 USDC。两者均未模拟入场执行，不能冒充实际账户收益。
- 新版旧批次报告保存为 `results/late-resolution-winner-trading/server-low-latency-003-5m/report-sensitivity-audit.txt`。旧数据没有研究组决策、历史跨度字段或新执行延迟字段，不能回补不存在的实时组间成交对照。

新组尚未在服务器累计实时样本，下一步是部署后验证，不自动进入实盘。
