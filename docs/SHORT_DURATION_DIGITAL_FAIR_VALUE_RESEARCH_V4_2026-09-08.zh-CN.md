# Short Duration Digital Fair Value v4 研究运行说明

## 修改目的

run-004 的三笔模拟成交全部亏损，模型将低价结果估计为接近一半或更高胜率；大量临近结算的盘口缺少一侧。此次扩展用于解释概率偏差并比较不同入场时间，暂不根据三个样本调整波动率下限或交易额度。

## 修改内容

- 支持 `SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_CHECKPOINT_SECS=10/30/60`。含义是每个五分钟市场最后 N 秒内持续扫描，仍为每个市场最多一次机会尝试。不是同时在三个时点分别下单；60 秒组可能在更早时已经尝试过。
- 执行版本标记为 4。
- 每个市场窗口第一次模型就绪时记录 `model_observation`，不要求已有交易机会。机会和成交也记录模型审计：实测波动率、下限、下限是否生效、开盘参考样本时间偏差、标的 feed 年龄、观察数量、去掉下限后的诊断概率、相对参考价方向。模型观察事件为 UP 视角，机会及成交为所买方向视角。去掉下限的概率只用于研究。
- 先使用原始盘口判断完整性、价格和点差，再检查过滤后的可执行档位与深度。无满足价格/费用后收益要求的档位不再误记成盘口缺失。最低卖价低于准入价格时拒绝该交易，不跳过低价档位去模拟买更贵档位。
- 延迟拒绝事件保存原始与过滤后的卖价档数、可执行最优卖价、可执行深度及模型审计。
- 报告增加 `research_audit`，汇总事件数量、窗口拒绝计数、市场窗口最终状态、REST 延迟与逐资产模型审计。最终状态并不代表窗口内每次扫描都相同。
- 报告增加含费用投入金额和已结算投入收益率，按入场均价、参考价方向、研究周期和总执行延迟分组。旧数据缺少的审计/周期字段标为未知，不根据目录猜测。REST 延迟和包括固定等待的总执行延迟分别统计。

审计和模拟价格基于现有数据源；参考样本时间偏差可被观察，但最终结算参考价口径是否完全匹配仍需后续证据。新增观察是定价就绪样本，不能当作无偏的全部市场概率校准样本。

## 默认部署

将新编译的 `target/release/short_duration_digital_fair_value` 和 `.env` 复制到服务器。在 `.env` 所在目录运行：

```bash
./short_duration_digital_fair_value dry-run
```

本次 `.env` 默认使用：

```dotenv
SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_CHECKPOINT_SECS=10
SHORT_DURATION_DIGITAL_FAIR_VALUE_LOG_FILE=logs/short-duration-digital-fair-value/server-low-latency-005-5m-10s.log
SHORT_DURATION_DIGITAL_FAIR_VALUE_RESULTS_FILE=results/short-duration-digital-fair-value/server-low-latency-005-5m-10s/events.ndjson
SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_REPORT_FILE=results/short-duration-digital-fair-value/server-low-latency-005-5m-10s/report.json
```

同目录运行 `./short_duration_digital_fair_value report` 即生成默认 10 秒组报告。

## 30 秒、60 秒对照组

在独立终端或独立服务中运行下列命令。30 秒使用 `sddfv_checkpoint=30`，60 秒使用 `sddfv_checkpoint=60`，其余命令相同：

```bash
sddfv_checkpoint=30
SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_CHECKPOINT_SECS="$sddfv_checkpoint" \
SHORT_DURATION_DIGITAL_FAIR_VALUE_LOG_FILE="logs/short-duration-digital-fair-value/server-low-latency-005-5m-${sddfv_checkpoint}s.log" \
SHORT_DURATION_DIGITAL_FAIR_VALUE_RESULTS_FILE="results/short-duration-digital-fair-value/server-low-latency-005-5m-${sddfv_checkpoint}s/events.ndjson" \
SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_REPORT_FILE="results/short-duration-digital-fair-value/server-low-latency-005-5m-${sddfv_checkpoint}s/report.json" \
./short_duration_digital_fair_value dry-run
```

相应报告命令：

```bash
sddfv_checkpoint=30
SHORT_DURATION_DIGITAL_FAIR_VALUE_RESULTS_FILE="results/short-duration-digital-fair-value/server-low-latency-005-5m-${sddfv_checkpoint}s/events.ndjson" \
SHORT_DURATION_DIGITAL_FAIR_VALUE_DRY_RUN_REPORT_FILE="results/short-duration-digital-fair-value/server-low-latency-005-5m-${sddfv_checkpoint}s/report.json" \
./short_duration_digital_fair_value report
```

目录自动创建；同一组重启仍追加记录。新的代码/参数实验应更换批次名。三个进程会增加网络请求，同一市场的三组样本有关联，应按市场配对比较，不能简单合并成独立成交。默认费用、1 秒执行等待、流动性折扣和单笔 5 USDC 保持不变。

先检查一轮完整市场窗口是否产生 `model_observation`；运行 24 小时后比较三组机会、盘口拒绝、波动率下限生效比例和模拟收益，再决定是否延长至 72 小时。研究周期支持不表示 30/60 秒组已通过盈利验证。

## 本地验证

15 项 Digital Fair Value 测试及 35 项共享 Late-Resolution Winner 相关测试通过，覆盖原始/过滤盘口拒绝原因、低于价格下限档位、审计字段与历史汇总兼容性。release 二进制已编译。

已用新版报告程序读取 run-004 原始记录并另存 `results/short-duration-digital-fair-value/server-low-latency-004-5m/report-v4.json`：3 笔、投入 15 USDC、净收益 -15 USDC 保持一致，总执行延迟分组为 1001–2000ms。没有重写原始日志和事件。30/60 秒研究组尚未在服务器运行。
