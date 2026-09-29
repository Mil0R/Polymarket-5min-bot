# Short Duration Digital Fair Value：008 最新运行分析

分析时间：2026-09-19（北京时间）  
运行方式：`SDDFV_BINARY=./target/release/short_duration_digital_fair_value bash scripts/run_digital_fair_value_frozen_008.sh start`  
分析对象：`server-low-latency-008-5m-30s`

本次只读分析。为保证报告与事件文件一致，重新执行了：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value bash scripts/run_digital_fair_value_frozen_008.sh verify
SDDFV_BINARY=./target/release/short_duration_digital_fair_value bash scripts/run_digital_fair_value_research.sh 30 server-low-latency-008 report
SDDFV_BINARY=./target/release/short_duration_digital_fair_value bash scripts/run_digital_fair_value_research.sh 30 server-low-latency-008 calibrate
```

## 结论

1. **本轮只增加了观测，没有增加成交证据。** 事件从 576 条模型观测增加到 627 条（+51），窗口从 194 增加到 211（+17），但 `shadow_fill` 与 `resolution` 都仍为 19 条，模拟净 PnL 仍为 **+119.77 USDC**。
2. **+119.77 仍不能证明策略可行。** 它是 19 笔模拟成交、7 胜 12 负的结果，且此前已经显示去掉最好的两笔后为负；本次新增的约 85 分钟没有改善这个集中度问题。
3. **严格可用子集反而是负的。** `eligible` 仅 4 笔、0 胜、-20 USDC，Brier 0.3696；canary gate 仍为 `ready=false`。因此不能进入真实资金 canary，也不应因总体 PnL 为正而放宽门槛。
4. **概率模型的主要问题没有改变。** 3 个资产的 627/627 条模型观测仍触发波动率下限；有可交易报价的 paired 子集上，模型 Brier 仍高于市场中价基准。
5. **冻结验证尚未完成。** 当前事件只覆盖 UTC 的 2026-09-18、09-19 两天，且 `run_started` 只有一个 run；原 manifest 要求至少 5 个 UTC 日。即使当前进程持续到窗口结束（09-21 02:05Z），这份文件最多覆盖 4 个 UTC 日，不能通过该门槛。

## 最新数据快照

来源：`results/short-duration-digital-fair-value/server-low-latency-008-5m-30s/events.ndjson`。

| 指标 | 上次分析（576） | 最新（627） | 变化 |
|---|---:|---:|---:|
| 观察时间 | 09-18 09:44:30Z–09-19 01:44:30Z | 09-18 09:44:30Z–09-19 03:09:30Z | +约 1小时25分 |
| `window_summary` | 194 | 211 | +17 |
| 模型观测 | 576（BTC/ETH/SOL 各192） | 627（各209） | +51（各+17） |
| paired 观测 | 80（29/20/31） | 87（31/21/35） | +7 |
| `opportunity` | 33 | 34 | +1 |
| `shadow_fill` | 19 | 19 | 0 |
| `resolution` | 19 | 19 | 0 |
| 结算胜负 | 7/19 | 7/19 | 0 |
| 模拟净 PnL | +119.7674 | +119.7674 | 0 |
| `shadow_rejected` | 14 | 15 | +1 |
| REST book audit | 31,410 | 34,298 | +2,888 |

当前 `status` 输出为：

```text
status=COLLECTING observations=627 paired=87 fills=19 resolutions=19
first=2026-09-18T09:44:30.062071348Z
last=2026-09-19T03:09:30.099428747Z
```

新增的第 34 个机会最终因 `delayed_price_out_of_range` 或 `delayed_no_executable_asks_after_costs` 被拒绝（新增段合计没有 fill）。这说明延迟后的可执行盘口仍是瓶颈，而不是扫描频率不足。

## 最新报告结果

来源：`report.json`（本次重新生成）。所有 PnL 均是 shadow 模拟结算，不是真实成交。

| 口径 | 样本 | 胜率 | Brier | 模拟 PnL |
|---|---:|---:|---:|---:|
| overall | 19 | 36.84%（7胜） | 0.2792 | **+119.77** |
| eligible | 4 | 0%（0胜） | 0.3696 | **-20.00** |
| eligible BTC | 3 | 0% | 0.2548 | -15.00 |
| eligible SOL | 1 | 0% | 0.7138 | -5.00 |

eligible 只占已结算样本的 4/19，样本太少，但它是预先定义的 canary 口径，结果明确未通过：`all_assets_ready=false`、`positive_total_pnl=false`、`ready=false`。整体 +119.77 主要来自 `gt_2s` 的 15 笔 research-only 样本（+139.77），而不是 ≤2 秒的 eligible 样本。

### 概率校准

最新 `calibration-report.json`：627/627 有结算标签，缺失 0、重复排除 0。

| paired 分组 | 样本 | 模型平均 p | 实际胜率 | 模型 Brier |
|---|---:|---:|---:|---:|
| BTC，>2s | 31 | 0.5304 | 48.39% | 0.2228 |
| ETH，>2s | 20 | 0.5132 | 40.00% | 0.1992 |
| SOL，>2s | 32 | 0.4849 | 37.50% | 0.2440 |

相较 576 条时的 BTC 29、ETH 20、SOL 31，新增样本没有改变结论：模型在可交易 paired 子集上仍没有显示出相对市场中价的优势。三资产 627/627 条观测均为 `floor_active`，说明当前概率主要由常数波动率下限驱动，而不是由实时波动估计驱动。

终值残差 RMS（>2s）为 BTC 2.93 bps、ETH 3.73 bps、SOL 4.80 bps，仍低于当前下限在 30 秒对应的隐含尺度 BTC 4.38、ETH 6.57、SOL 8.22 bps。下限过宽的问题依旧存在，但不能简单删除下限；应改为按资产×剩余时间的终值残差分布校准。

## 执行质量与拒绝漏斗

- REST 平均延迟 **25.93 ms**，最大 **3,698 ms**；有 2 次在信号前超过延迟上限。
- 37 次 `rest_fallback_completed_after_window`，说明 REST 回退成功不等于仍可执行。
- 13 次交易所时间戳过期、1,965 次 `stale_feed`。
- 窗口级拒绝仍以盘口问题为主：`up/down_incomplete_book` 为 16,816/16,815；另外有 2,363/2,124 次价格越界和 2,187/2,419 次价差过宽。
- WebSocket 在本段日志中没有新增错误；但大量 incomplete book 使有效机会率很低，不能仅据此判断网络健康等于策略可交易。

新增 17 个窗口中没有 fill，反而强化了此前判断：当前主要问题是“模型产生的 edge 是否真实”与“延迟后是否仍有可执行卖价”，不是扫描频率或 REST 平均延迟。

## 与更早历史结果的对比

此前 9 个批次的汇总是 331 笔模拟成交、95 胜、累计 **-273.04 USDC**；其中低价成交（<0.30）贡献了全部亏损。008 当前 19 笔的正值与这个历史结论并不矛盾：它的样本很少、低价凸性很强，且正收益集中在少数成交。

需要纠正一个口径问题：旧文档中出现过“008 为 29 笔、+114.72 USDC”的数字，那是此前另一段已被替换/混合的事件快照，不能与当前 `d6f3429f-6ea5-416f-b1ee-f31d649fc2d8` 这一份连续事件文件直接合并。当前可验证文件的权威数字是 **627 观测、19 笔成交、+119.77 USDC**；备份文件只证明了 576→627 的追加，不包含一个可恢复的 5 天完整窗口。

## 是否需要继续运行

可以继续运行以收集校准观测，但应把它当作“未完成的研究采样”，不能把新增无成交时段计入盈利证据。下一步按优先级：

1. **先恢复或确认 09-14–09-17 的原始事件**。若服务器不存在，当前 008 应标记为不满足 5 UTC 日门槛，并重新开启一个带独立批次目录的冻结窗口；不要用重复备份伪造覆盖天数。
2. 保持事件 append-only，继续生成 `report`/`calibrate`，但只按 `eligible` 口径做 canary 判断。
3. 在新冻结窗口中替换“常数下限概率”为资产×剩余时间的终值残差校准，并预注册卖价、费用、滑点和部分成交规则。
4. 在 `eligible` 每资产达到至少 50（最终冻结要求每资产 paired ≥75、至少 5 个 UTC 日、含费成交 ≥100）之前，不进入真实资金运行。

当前判断：**研究价值仍有，但不是实盘价值；不能因为 overall +119.77 而放行。**

