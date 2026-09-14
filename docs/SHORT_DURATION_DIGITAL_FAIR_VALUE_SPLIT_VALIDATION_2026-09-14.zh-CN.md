# Short Duration Digital Fair Value：时间留出验证

本次新增只读验证入口 `validate`。它读取当前批次的 `events.ndjson` 和 `reference-audit.json`，按市场开始时间升序将完整市场分配为 60% train、20% validation、20% holdout；同一市场的资产不会跨分区。当前不拟合、不修改、不自动推广任何参数。

## 运行

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_research.sh 10 server-low-latency-007 validate
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_research.sh 30 server-low-latency-007 validate
```

结果分别写入：

- `results/short-duration-digital-fair-value/server-low-latency-007-5m-10s/calibration-split-report.json`
- `results/short-duration-digital-fair-value/server-low-latency-007-5m-30s/calibration-split-report.json`

服务器若尚未有官方结算缓存，默认 `validate` 会立即给出缓存路径和补齐命令，不会误把缺失标签当成零收益。可在具备 Gamma 网络访问的环境中显式刷新：

```bash
SHORT_DURATION_DIGITAL_FAIR_VALUE_RESULTS_FILE=results/short-duration-digital-fair-value/server-low-latency-007-5m-10s/events.ndjson \
  ./target/release/short_duration_digital_fair_value validate --refresh
```

注意脚本通过环境变量传递刷新开关（因为脚本命令本身只接受 `validate`）：

```bash
SDDFV_VALIDATE_REFRESH=1 SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_research.sh 10 server-low-latency-007 validate
```

在网络受限服务器上，建议先在可联网机器运行 `calibrate --refresh`，再把对应的 `reference-audit.json` 连同事件目录复制到服务器或本地分析机。

## 007 批次结果

每个期限共有 276 个市场，每个资产在 train/validation/holdout 分别为 165/55/56 个观察；标签缺失为 0。

| 期限／分区 | 当前模型 Brier | 无下限 Brier | 当前模型 Log loss | 无下限 Log loss |
|---|---:|---:|---:|---:|
| 10s train | 0.06725 | 0.02651 | 0.24117 | 0.21221 |
| 10s validation | 0.07455 | 0.01927 | 0.27268 | 0.07413 |
| 10s holdout | 0.04662 | 0.01978 | 0.17887 | 0.14559 |
| 30s train | 0.10773 | 0.06700 | 0.35779 | 0.60468 |
| 30s validation | 0.11854 | 0.07551 | 0.39779 | 0.72289 |
| 30s holdout | 0.08694 | 0.08591 | 0.29666 | 0.44662 |

市场中点只有部分观察有值，因此仅作为可用时的参考基线，不能和完整模型样本直接横向比较：10s holdout 8 个，30s holdout 32 个。高概率（模型概率 ≥0.9）样本在本批次的 holdout 恰好全部获胜，但每资产仅 6–14 个，不能视为校准已通过，也没有覆盖实际成交样本的成本和延迟。

## 解释与下一步

- 007 的时间留出没有证明当前模型具有可交易优势；尤其 30s holdout 的 Brier 仍约 0.087，和本批交易亏损一致地提示需要校准。
- 无下限方案在 Brier 上改善，但 30s 的 log loss 明显恶化，说明它可能变得过度自信；不能直接把“无下限”部署到 dry-run。
- 下一步应在 train 中拟合受约束的资产／剩余期限校准器，在 validation 选择，而后只在后续新批次 holdout 验证。不得根据 007 holdout 结果反向调参。
- 需要把盘口可用、参考行情年龄、买入价格和成本后反事实收益单独分层；概率评分通过也不等于订单可成交。

本报告是研究诊断，不能证明策略可实盘，也不改变原始 dry-run journal 和现有参数。
