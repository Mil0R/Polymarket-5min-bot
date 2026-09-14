# NegRisk Conversion Arbitrage：Shadow Run 003 优化

本轮落实 `shadow-run-002` 分析后的剩余工程建议，仍为公开数据只读 Shadow。没有签名器、订单提交或链上 convert；`protocol_verified=false`、`live_ready=false` 和经济性晋级门槛保持不变。

## 路线覆盖

- 每个 Gamma 分页范围分别保存路线游标。当合格路线超过 `MAX_ROUTES` 时，不再永久保留固定的前 100 条，而是在该范围下一次刷新时从上次结束位置继续。
- 路线在轮换前按费用腿数、组合腿数、最早截止时间和 route ID 稳定排序。排序用于提高低成本、较小组合的早期可见性；轮换保证排序靠后的路线仍会进入采样。
- `discovery` 记录 `route_selection_offset`、`next_route_selection_offset`、`routes_truncated`、本轮新见路线和运行期累计唯一路线。
- 报告增加逐分页范围的扫描次数、单轮最大候选数、被截断轮次和实际发出过的唯一路线数，同时保留 repeated-observation 累计计数与 peak 值，避免把重复轮次当作独立路线。

## 活跃度与元数据

- 每条路线保存事件标题、市场数量、启用费用的腿数、NegRisk Other 数量，以及最早/最晚 deadline。
- 每次盘口扫描按完整盘口腿数、严格新鲜腿数、有效顶档数量、最弱顶档深度、原始顶档价差和费用腿数生成研究排序。
- 盘口证据采用“一半优先级、一半轮换”的采样方式。优先部分关注当前更完整、更活跃且深度较好的路线，轮换部分防止长期只保存少数热门路线。
- 研究排序只控制证据保存优先级，所有当轮已选择路线仍执行完整经济性检查，不因排序绕过任何腿。

## 成本审计

候选、诊断候选以及每轮最佳非候选现在保存以下固定数量下的场景：

- 不含手续费、滑点、转换费和链上成本；
- 只含实际市场费表；
- 手续费加配置滑点；
- 再加入假设转换费；
- 完整配置；
- 双倍滑点压力。

同时输出市场手续费、滑点、转换费、链上成本和额外滑点压力的增量损耗。这些分解使用相同盘口和同一数量，便于判断原始价差究竟被哪类成本吃掉。Gamma 费表仍是行情元数据，必须等经济性通过后再用真实订单回执核验。

## 接近盈亏平衡路线复核

- 不提高全部路线的轮询频率。仅当诊断口径净收益达到配置下限时，选取最接近盈亏平衡的有限路线。
- 默认对净收益不低于 `-$0.10` 的前 5 条路线，在 2 秒后重复获取盘口。
- `focused_observation` 保存首次/复核报价、是否转为正收益、成本敏感性以及复核盘口证据。该事件只是研究证据，不会被当成可执行成交。
- 若 run 003 的最佳净收益仍稳定在此前约 `-$0.05`，这组数据可区分“短暂偏差很快收敛”与“成本后结构性负值”；无需为大量明显负收益路线增加 API 压力。

## 报告与日志隔离

- `report` 默认选择目录中最后一个 `run_started` 对应的 run_id 进行经济性汇总。
- 历史运行的事件计数、起止时间和是否被选中保存在 `run_summaries`；整个目录的累计事件计数单列为 `directory_event_counts`。
- 旧运行的 discovery 错误不会进入最新运行的 `event_counts`、拒绝原因和最佳报价。
- `run_started` 保存包版本、可选构建提交和研究 schema 版本。
- 运行消息同时写入独立 `runtime.log` 和该策略的系统 tracing 日志，控制台只保留一份输出。

用新版报告器重读 `shadow-run-002` 后，正确选择最新 `7f18f664-...` 运行；早期 1,028 条错误只存在目录/旧 run 汇总中。旧事件没有新成本字段，报告不会伪造历史时点未记录的数据。

## 新批次配置与运行

本地 `.env` 已切换为：

```dotenv
NEG_RISK_CONVERSION_ARBITRAGE_RUN_DIR=results/neg-risk-conversion-arbitrage/shadow-run-003
NEG_RISK_CONVERSION_ARBITRAGE_SYSTEM_LOG_FILE=logs/neg-risk-conversion-arbitrage/shadow-run-003/system.log
NEG_RISK_CONVERSION_ARBITRAGE_FOCUS_ENABLED=true
NEG_RISK_CONVERSION_ARBITRAGE_FOCUS_NET_PROFIT_FLOOR_USDC=-0.10
NEG_RISK_CONVERSION_ARBITRAGE_FOCUS_DELAY_MS=2000
NEG_RISK_CONVERSION_ARBITRAGE_FOCUS_ROUTES=5
```

其他核心经济参数保持 run 002 口径：100 路线/范围、最多 6 个结果、5/10/25 份、25 USDC 输入预算、0.05 USDC 最低净收益、10bps 滑点、0.02 USDC 链上成本，以及独立诊断组。单纯扩大资金或取消成本不会改变负单位收益。

编译和运行：

```bash
cargo build --release --bin neg_risk_conversion_arbitrage
./target/release/neg_risk_conversion_arbitrage shadow
```

服务器复制 `.env` 与 `target/release/neg_risk_conversion_arbitrage` 后：

```bash
./neg_risk_conversion_arbitrage shadow
```

正常停止后生成最新 run 报告：

```bash
./neg_risk_conversion_arbitrage report
```

建议运行 48–72 小时。重点查看 `discovery_page_coverage`、`latest_route_research_ranking`、`best_non_candidate.cost_sensitivity`、严格/诊断候选及其 2/5/10 秒退出。若覆盖改善后仍没有净正值，应将模块降级为低频监控并停止执行层开发。

## 验证

- 316 项库测试全部通过。
- 新增测试覆盖截断路线跨轮完整覆盖、成本分解回算和多 run 报告隔离。
- release 二进制编译成功。
- 公开接口只读冒烟成功发现 5 条转换路线，生成新 discovery 覆盖字段、路线元数据、研究排序和 run 隔离报告。本次短冒烟盘口返回为 0，因此成本场景由确定性盘口单元测试验证，未把缺失盘口解释为零收益。

链上 Adapter、抵押资产、indexSet 和真实转换输出仍未验证。这一步继续受经济性门槛约束，不能由 Shadow 路线发现成功自动开放。
