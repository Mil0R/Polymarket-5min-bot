# Short Duration Digital Fair Value 执行修正

本次针对 AWS `server-low-latency-003-5m` 样本暴露的执行问题进行修正。

- REST fallback 返回后及模拟执行盘口返回后，重新读取 Chainlink 数据，以最新价格、波动率及剩余时间计算概率。缺失、过期模型或已经结束的窗口不执行。
- 执行阶段复用入场盘口检查：买卖价完整、价差、价格范围及顶部深度。缺失 bid 不再被直接模拟成交。
- 多档盘口撮合按最终模拟均价、滑点和费用约束预算；`ORDER_USDC` 是含费支出上限。预算约束后的成交比例继续接受最低部分成交比例检查。
- 结算读取要求 closed 且 outcome prices 明确为 `[1,0]` 或 `[0,1]`，否则继续等待。该规则是终局价格确认，不是链上赎回确认。
- `shadow_fill` 新增 `signal_probability`、`execution_model_observed_at_ms`，原 `probability` 记录执行阶段概率，报告按执行概率校准。

历史事件不重写。此前样本中的收益不能视为修正后的回测结果；新旧运行应使用不同日志及结果目录。

验证：`cargo test --lib digital_fair_value::tests`，覆盖概率随新报价更新、模型缺失/窗口过期、含费预算与终局价格校验。

部署命令：

```bash
cargo build --release --bin short_duration_digital_fair_value
./target/release/short_duration_digital_fair_value dry-run
./target/release/short_duration_digital_fair_value report
```

本次不代表大量 incomplete_book 或 stale_feed 的根因已解决，后续仍需结合盘口响应和数据时间戳验证。

## 2026-09-07 后续优化（execution_version=3）

- REST fallback 追加 `rest_book_audit`：请求/响应 token、市场、交易所时间戳、买卖档数、响应解析耗时。响应 token 不匹配则拒绝；时间戳缺失仍需分析，不能把 HTTP 成功率当作可用盘口比例。
- 执行深度改为限价及净边际约束内、流动性折扣后的累计 ask 深度。最优档较薄但后续档位足够时允许评估。每档价格都要满足滑点后价格上限与净边际。
- 成交事件记录执行版本、预算及 execution_audit（开盘参考、当前价格、波动率、剩余时间、总执行延迟、盘口元数据）。结算追加终局价格证据，超时追加 resolution_pending。
- 报告按运行、版本、盘口来源分组，结算去重，剔除无成交记录的结算。报告标记超预算、旧版本、重复成交/结算、孤立结算及未结算样本；这些问题阻止 canary。旧版缺失预算字段按历史默认 $5 审计。
- Canary 校准要求逐资产通过 Brier 门槛，不仅检查总体平均值。
- `.env` 新批次：日志 `logs/short-duration-digital-fair-value/server-low-latency-004-5m.log`，事件及报告在 `results/short-duration-digital-fair-value/server-low-latency-004-5m/`。

仍需验证：开盘参考与真实结算口径、盘口来源与时间戳可靠性。此更新未证明模型校准成功，也不为旧日志生成修复后的反事实收益。
