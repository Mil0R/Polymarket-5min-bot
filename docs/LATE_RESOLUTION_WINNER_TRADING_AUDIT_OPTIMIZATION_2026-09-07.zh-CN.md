# Late-Resolution Winner Trading 审计优化（2026-09-07）

## 背景

`server-low-latency-002-5m` 包含两次 PAPER 运行、387 次评估。正式订单为空；完整确认后 Shadow 共 75 次尝试、57 次模拟成交，模拟净收益 -3.20856230 USDC。报告旧版选定评估关联的 22 笔盈利子集不能代表完整 Shadow 表现。

日志最后的策略汇总为 2026-09-06 17:46 UTC，行情连接记录持续到次日 01:15 UTC。历史文件不能确定停滞根因，不能把这个时间差直接算作有效扫描时长。

## 实现内容

1. PAPER 运行增加独立系统线程监控汇总心跳与主事件循环心跳。每分钟检查一次；汇总停滞超过 `max(3 × 汇总周期, 180 秒)`、或主循环停滞超过 1800 秒时告警。主循环阈值容纳正常等待 15 分钟市场窗口。恢复、主动关闭、汇总任务异常退出都有日志。此监控提供定位证据，不自动重启交易或补单，也不能在整个进程被终止后继续告警。
2. 修复逆向走势概率被限制为至少 50% 的问题。概率范围改为 0.01%–99.99%；逆向走势仍受反转边界和净期望检查限制。
3. 每条可定价评估新增 `model_audit`：开盘样本时间、相对窗口起点偏差、当前样本年龄、观察数、实测波动率、波动率下限、下限是否生效，以及去掉下限后的诊断概率。诊断概率不参与交易准入。旧评估文件可继续读取，缺失审计字段表示未知。
4. 报告优先显示完整确认后 Shadow 统计，标明分组重叠、尝试胜率与模拟成交胜率的分母区别，并增加 `shadow_wins`。次要反事实区明确标记关联 Shadow 是子集。
5. 报告汇总全部失败条件，避免主拒绝原因掩盖 EV 失败；增加资产与模型概率区间的已结算样本校准，包括平均预测概率、实际胜率和 Brier 分数。该样本经过候选筛选，不是独立样本外盈利验证。
6. 缺少模型输入的评估不再将占位概率零计入概率偏差统计；新记录标为 `model_unavailable`。新旧报告概率统计分母可能因此不同，全部评估数量和 Shadow 收益不受影响。

交易门槛、风险缓冲、额度及 `.env` 保持原值。BTC/ETH、价格与剩余时间分组继续保留；本次不根据历史盈利子集筛选上线。

## 本地验证

- 35 项 Late-Resolution Winner 模型、记录及执行相关库测试通过。
- 3 项报告测试通过，新增覆盖未成交市场的胜负不会进入模拟成交收益、亏损成交会正确冲减收益。
- 新版报告成功读取原批次：57 笔模拟成交中 55 胜、2 负，净收益仍为 -3.20856230 USDC。
- 排除 20 条缺少模型输入的评估后，有效概率预测为 367 条，平均模型减盘口概率为 -0.35895454，即低约 35.90 个百分点。旧版低约 39.08 个百分点包含占位值，不能沿用该口径。差距依然明显，尚需新增审计数据解释。
- 历史文件的 `model_audit` 样本为零，新版报告明确提示缺失，未将其解释为“波动率下限未生效”。

## 部署与下一轮验证

`.env` 已将本次优化后的 5 分钟批次切换为 `server-low-latency-003-5m`：

```dotenv
LATE_RESOLUTION_WINNER_TRADING_STATE_JOURNAL_FILE=results/late-resolution-winner-trading/server-low-latency-003-5m/orders.ndjson
LATE_RESOLUTION_WINNER_TRADING_EVALUATION_JOURNAL_FILE=results/late-resolution-winner-trading/server-low-latency-003-5m/evaluations.ndjson
LATE_RESOLUTION_WINNER_TRADING_STRATEGY_LOG_FILE=logs/server-low-latency-003-5m/late-resolution-winner-trading.log
```

使用新编译的 `target/release/late_resolution_winner_trading`，在配置所在目录运行：

```bash
./late_resolution_winner_trading dry-run
```

以上命令沿用 `.env` 中的周期和新批次路径。需要临时使用其他批次名称时，可以通过进程环境覆盖：

```bash
LATE_RESOLUTION_WINNER_TRADING_STATE_JOURNAL_FILE=results/late-resolution-winner-trading/model-audit-20260907-5m/orders.ndjson \
LATE_RESOLUTION_WINNER_TRADING_EVALUATION_JOURNAL_FILE=results/late-resolution-winner-trading/model-audit-20260907-5m/evaluations.ndjson \
LATE_RESOLUTION_WINNER_TRADING_STRATEGY_LOG_FILE=logs/model-audit-20260907-5m/late-resolution-winner-trading.log \
./late_resolution_winner_trading dry-run
```

路径中的 `5m` 仅是目录名称，不切换实际市场周期；上例用于当前默认 5 分钟配置。目录由日志与记录组件创建。

用同批次评估文件生成报告，程序会读取同目录 `orders.ndjson`：

```bash
./late_resolution_winner_report results/late-resolution-winner-trading/model-audit-20260907-5m/evaluations.ndjson
```

报告输出至控制台，需要留存时可追加 `| tee report.txt`。运行数据、订单和策略日志仍由各自记录器保存。

下一轮先确认周期汇总和主循环持续工作，再用新增审计字段判断概率差距来自参考价时间偏差还是波动率下限，并使用新批次独立检验 BTC/ETH 分组。旧数据无法恢复新审计字段；当前优化不构成盈利已验证的结论。
