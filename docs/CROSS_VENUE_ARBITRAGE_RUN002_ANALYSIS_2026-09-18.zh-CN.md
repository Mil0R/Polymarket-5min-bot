# Cross-Venue Arbitrage Run-002 报告生成与日志分析

> 运行 ID：`1e8f11d5-c25e-4af3-91f9-62d3c359e933`（`shadow`，run 目录 `shadow-run-002`）
> 分析日期：2026-09-18　本文档不修改代码、`.env`、登记表或原始事件

## 结论

最新一轮 shadow 采集是**运行稳定性最好的一轮，但经济性验证仍然是零**：42 小时 15 分内 1,262 次轮询全部成功，无中断、无重试、无损坏行；然而配对登记表始终为空，候选在 `unmapped_pair` 处被拒，因此 0 次盘口探测、0 条报价、0 个机会、0 成交。

本轮相比上一轮的唯一实质进展是**首次出现 8 条 `WATCH` 行并走通了标签筛选之后的漏斗**（产生 8 条 `candidate` 事件）。但逐条核对原始响应后可以确认：这 8 条来自**同一份供应商快照的数据缺失**（该行只有 Kalshi 价格、Polymarket 价格为 `null`），而不是真实的跨平台价差信号。也就是说，本轮"有信号"这件事本身是数据完整性问题，不是套利机会。

另一个必须记录的新发现是**供应商数据自洽性**：15,080 条双价位行中有 **3,138 条（20.8%）** 的 `cheaper_venue` 与其自身给出的两个百分比相互矛盾；`cheaper_venue` 在全部行中 97.1% 恒为 `polymarket`。在这条问题被解释清楚之前，`gap_pp` / `WATCH` 不能作为下单依据。

**下一步不应继续空跑采集**：登记 1–2 个已人工核对的事件（Kalshi ticker + Polymarket token + 费用上界 + `reviewed_digest`）以激活 `probe`/报价/延迟敏感性链路，同时就字段语义（尤其是缺价行的 `gap_pp` 与 `cheaper_venue`）向供应商求证。

## 1. 本次生成物

```bash
./target/release/cross_venue_arbitrage report
./target/release/cross_venue_arbitrage leads
```

| 文件 | 说明 |
|---|---|
| `results/cross-venue-arbitrage/shadow-run-002/report.json` | 本次新生成（该目录此前没有 report.json），29,508 B |
| `results/cross-venue-arbitrage/shadow-run-002/pair-leads.json` | 本次新生成，37 条待人工审核线索，25,669 B |
| `results/cross-venue-arbitrage/pairs.json` | 仍为 `[]`，`leads` 命令未改写登记表 |

报告选中最新 `run_id`（上表运行 ID），`selected_run_data_integrity_ok=true`、`malformed_lines` 为 0。

## 2. 运行与采集统计

| 指标 | 结果 | 含义 |
|---|---:|---|
| 记录跨度 | UTC 2026-09-16 12:06:09 → 2026-09-18 06:21:14（北京时间 09-16 20:06 → 09-18 14:21） | `elapsed_secs` 152,104 s ≈ 42 h 15 min |
| 轮询次数 | 1,262 | 全部 `feed_attempt` 状态 `success`，`attempt=1`，无重试 |
| 轮询节奏 | 中位 120.6 s（120.45–122.09 s） | 间隔 >180 s 的断点：**0 个**（无重启、无长时间卡顿） |
| PMP 请求延迟 | p50 562 ms / p95 984 ms / max 2,086 ms | 这是供应商 MCP 请求耗时，不是交易所盘口延迟 |
| 返回行 | 1,262 × 12 = 15,144 | `returned_equals_total_cycles=1262`，`truncated_unknown_cycles=1262` |
| 标签 | `ALIGNED` 15,136；`WATCH` 8；`ARB`/`ARB_FLAGGED` 0 | 全部在标签阶段被跳过或仅进入候选 |
| 供应商快照 | `unique_source_snapshots = 170` | 刷新节奏中位 **900.07 s**（874–925 s），即约 15 分钟 |
| 重复系数 | 15,144 行 / (170 × 12) = **7.42×** | 120 s 轮询主要是重复读取同一份快照：真实观测只有 2,040 行 |
| 来源年龄 | 中位 450 s，最大 907 s | 超过 900 s 的周期 1 个（0.08%）；`vendor_stale` 0 |
| 事件完整性问题 | 14 行损坏 0 个；`directory_malformed_lines=0` | 分段 NDJSON 无撕裂 |
| 订阅成本 | 14.99 USD/月；本窗口摊销 **0.88 USD** | 对应收入 0 |

## 3. 漏斗：机会为什么仍然是 0

```
15,144 行 ──标签筛选──▶ label_skipped 15,136 / candidate 8
        ──配对匹配──▶ unmapped_pair 8（registry_pairs 在所有 1,262 个周期均为 0）
        ──▶ selected_pairs 0 ──▶ 盘口探测 0 ──▶ 报价 0 ──▶ conditional 收益 0 ──▶ fills 0
```

- `registry_pairs=0` 出现在全部 1,262 个 `feed_summary` 中，即登记表在整个窗口内为空，不是"某些周期恰好没有配对"。
- `rest_latency={}`、`best_priced_quote=null`、`unique_pair_opportunities=0`、`robust_conditional_pairs=0`、`conditional_worst_delay_profit_sum=0`、`actual_fills=0`、`realized_pnl=0`、`live_ready=false`。
- 卡点已经**不再是标签**：上一轮 4,500 次扫描 5,400 行全部 `ALIGNED`、`WATCH` 为 0，连候选阶段都没进过；本轮 8 条 `WATCH` 成功进入候选并被执行了真实的"配对查找 → 拒绝"分支（`src/cross_venue_arbitrage/mod.rs` 中 `eligible_signal` → `pair.is_none()` → `unmapped_pair`）。**现在唯一的阻塞项就是登记表。**

## 4. 8 条 WATCH 的真相：缺价，而非机会

8 条候选全部是同一行、同一份快照：

```
as_of 2026-09-17T17:45:01.758Z（freshness=live, age 24s）
NFL · MIA Dolphins vs SF 49ers: gap +3.2pp — WATCH (Kalshi 90%)
结构化字段: {game:"MIA Dolphins vs SF 49ers", gap_pp:3.2, kalshi_implied_pct:90,
             polymarket_implied_pct:null, cheaper_venue:null, signal:"WATCH"}
```

8 次 `candidate` 事件的 `age_seconds` 为 23→867 s，正好是同一份快照在 900 s 年龄门槛内被 120 s 轮询重复读取 8 次，随后该行会因超龄退出。

关键事实：

1. **该行没有 Polymarket 价格**（`polymarket_implied_pct=null`、`cheaper_venue=null`）。即使存在登记配对，双边对冲也无法定价，`WATCH` 在这里是**数据完整性问题**而不是价差信号。
2. 同一场比赛在有双边价格的快照里，最大 `gap_pp` 仅 0.5pp、方向矛盾 0 次（99 份快照）。3.2pp 是瞬时的单边数据，不是持续价差。
3. `WATCH` 行与 `include_watch=true` 配置共同作用，让"缺一条腿"的行进入了候选阶段——这条路径在上一轮的报告里被当作未验证的假设，本轮验证结果是**它只会产出伪信号**。

### 缺价行的全量清单（本窗口共 64 行，占 0.42%）

只出现在 2 份供应商快照，每份 8 行、各被重复读取 8 次：

| 快照 `as_of` | 市场 | 显示的 `gap_pp` | Kalshi | 标签 |
|---|---|---:|---:|---|
| 2026-09-16T15:15:01.715Z | CLE Browns vs TB Buccaneers | 1.9 | 79% | ALIGNED |
| 2026-09-16T15:15:01.715Z | IND Colts vs KC Chiefs | 1.5 | 73% | ALIGNED |
| 2026-09-16T15:15:01.715Z | NO Saints vs BAL Ravens | 1.3 | 79% | ALIGNED |
| 2026-09-16T15:15:01.715Z | SEA Seahawks vs ARI Cardinals | 1.0 | 35% | ALIGNED |
| 2026-09-16T15:15:01.715Z | NY Giants vs LA Rams | 0.8 | 26% | ALIGNED |
| 2026-09-17T17:45:01.758Z | MIA Dolphins vs SF 49ers | 3.2 | 90% | **WATCH** |
| 2026-09-17T17:45:01.758Z | IND Colts vs KC Chiefs | 2.1 | 74% | ALIGNED |
| 2026-09-17T17:45:01.758Z | NY Giants vs LA Rams | 1.6 | 74% | ALIGNED |

这些行的 `gap_pp` **无法从响应自身字段复算**（缺少 Polymarket 侧价格），说明 `gap_pp` 来自响应里没有暴露的另一个数据源或缓存。该现象只能记录为待解释项，不能据此断定供应商错误，也不能据此下单。

## 5. 供应商数据自洽性审计（本轮新增发现）

对 15,080 条双边价位行做方向自洽检查（`cheaper_venue` 是否与两个百分比的大小关系一致）：

| 指标 | 数值 | 说明 |
|---|---:|---|
| 方向矛盾行（重复计） | **3,138（20.8%）** | 例如 `cheaper on polymarket (Kalshi 49% vs Poly 50%)` |
| 方向矛盾快照（去重） | 423 | 按唯一 `as_of` 去重 |
| `cheaper_venue` 分布 | polymarket 14,709 / kalshi 371 / null 64 | **97.1% 恒为 polymarket**，与价格方向无关 |
| 出现矛盾的比赛 | 25 / 35 | 集中在 Philadelphia vs New York M（305/530 = 57.5%）、NO Saints vs BAL Ravens（53.0%）、CIN Bengals vs HOU Texans（51.6%） |
| 无矛盾的比赛 | 如 MIA Dolphins vs SF 49ers（0/732）、LV Raiders vs LA Chargers（0/135） | 说明矛盾随市场而变，不是全局常量 |

补充的分辨率问题：`gap_pp` 是亚百分点精度（0.3、0.5、0.7…），而 `kalshi_implied_pct` / `polymarket_implied_pct` 是整数百分比，因此多数 0.5pp 级别的差在显示值上不可复算（例：`gap 0.5pp，Kalshi 89% vs Poly 89%`）。这意味着**当前审计只能发现粗矛盾，无法证伪细差**。

`gap_pp` 分布（全部 15,144 行）：0pp 4,562；0.5pp 1,454；1pp 7,553；2pp 192；3pp 44；3.2pp 8；4pp 15；其余为零星小数。**中位数约 1pp，最大 4pp**。

报告口径已经写明了正确解读：`displayed_direction_mismatch` 只在"两个百分比代表同一结果"的假设下成立，缺少精确 outcome 身份时不能单独证明供应商错误。本节的数字同样只能用于**决定是否继续信任这条数据源**，不能用于结论性判断。

## 6. 线索（leads）清单

- `pair-leads.json`：37 条待人工审核线索，`automatic_registry_write=false`，每一条的 `missing_identity_fields` 都是 `["event_date","explicit_outcome","kalshi_ticker","polymarket_slug","token_ids"]` 五项全缺，`review_status` 全为 `unresolved`。
- 其中只有 **1 条**被标为 `review_priority="eligible_signal"`：NFL `MIA Dolphins vs SF 49ers`（8 条 `WATCH` + 724 条 `ALIGNED`，99 份快照，最大显示差 3.2pp）——即第 4 节讨论的那条缺价行。
- 其余 36 条为 `diagnostic_only`。`max_displayed_gap_pp` 排名靠前的（Chicago WS vs Cleveland 4.0pp、CIN Bengals vs HOU Texans 3.0pp）恰好也是方向矛盾最多的市场，**不能直接当作候选机会**；其中 "Chicago WS vs Cleveland" 的 4pp 只出现 15 行且同市场存在 15 行方向矛盾。

## 7. 与上一轮的对比

| 维度 | shadow-run-001 | shadow-run-002（本轮） |
|---|---|---|
| 成功扫描 / ALIGNED 行 | 243 + 207 = 450 / 5,400（两段） | 1,262 / 15,136（单段连续） |
| WATCH 行 | 0 | **8**（同一缺价快照 ×8 次重复） |
| 候选事件 | 0 | **8**，全部 `unmapped_pair` |
| 登记配对 | 0 | 0（无变化） |
| 时间连续性 | 分段运行，中间有停止 | **42.25 h 连续，无中断** |
| 数据质量审计 | 未单列方向矛盾 | **20.8% 方向矛盾、97.1% 单边 `cheaper_venue`、64 条缺价行** |
| 经济性证据 | 0 | 0 |

进展是"工程与数据审计"，不是"策略收益"。

## 8. 建议

1. **停止空跑**。再跑一轮无登记的 shadow 只会重复产出同一批 `ALIGNED` 行；登记表是唯一阻塞项。
2. **先登记 1–2 个事件打通链路**。选规则最清晰的市场（同一比赛日期、同一结算口径、无平局/加时歧义），补全 `kalshi_ticker`、`poly_slug`、两个 outcome token、`poly_fee_cap_per_net_share`、`kalshi_fee_cap_per_contract`、`review_notes`、`valid_until`，跑 `inspect` 生成指纹并回填 `reviewed_digest`，再跑 `probe` 验证真实盘口（`probe` 不依赖 PMP）。
3. **把缺价行排除在可交易集合外**（第 4 节的 8 条 `WATCH` 已证明是伪信号），或至少要求 `polymarket_implied_pct` 与 `kalshi_implied_pct` 同时非空才计入候选。这是策略门控问题，需要确认后再改代码。
4. **向供应商求证三项字段语义**：缺价行的 `gap_pp` 从何而来；`cheaper_venue` 的判定依据（当前 97.1% 恒为 polymarket）；整数百分比与 `gap_pp` 小数位的舍入关系。在这些澄清之前，`ARB`/`ARB_FLAGGED` 标签也不应被信任。
5. **降采样或按 `as_of` 去重**。供应商 900 s 刷新、我们 120 s 轮询 → 7.42× 重复写盘（本轮 3 天分段共 7.9 MB 事件）。按 `as_of` 变化才记录可显著缩小归档，同时保留异常快照的完整证据。
6. **对经济性保持悲观预期**：目前可见的跨平台显示差中位数只有 1pp，而 Kalshi 侧存在按合约计的费用、Polymarket 侧需要 `merge/split` 操作，另有 `min_net_per_pair=0.02 USD`、`slippage_bps=20` 的门槛。即使配对登记完成，1pp 级别的显示差很可能在费用与滑点之后不再剩下净收益；`probe` 的作用正是尽早证伪这一点。

## 9. 不可从本次数据推断的内容

- 不能推断跨平台套利不存在：登记表为空、盘口从未被真实读取，缺失映射不构成"无套利"证据。
- 不能推断供应商数据错误：方向矛盾只在"同一 outcome"假设下成立，而响应缺少 outcome 身份。
- 不能推断服务端崩溃或仍在运行：本地事件止于 2026-09-18 06:21:14 UTC，无 `run_stopped` 事件，仅代表本地同步数据到此为止。
- `actual_fills` / `realized_pnl` 为 0 是只读程序的固定输出，不是交易成绩。
- 报告的 `conditional_*` 字段（本轮全为 0）是"若按报价成交"的条件情景，不是收益。
