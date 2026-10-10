# SDDFV 条件终值分布校准：实现与本地验证

日期：2026-10-01。适用 BTC、ETH、SOL 的 5 分钟市场。本轮模型只包含 30 秒检查点；不应直接用于 10 秒、60 秒或 15 分钟市场。

## 结论

本轮已完成 SDDFV 独立的终值残差估计、条件概率模型、时间顺序验证、延迟成交回放及 Rust/Python 推断一致性验证。旧的 Gaussian-floor 模型保留为对照，不再通过直接调小波动率下限来制造更大的模型优势。

本地研究纳入 3,622 个已取得终值标签的有效样本，3,598 个具备全部条件特征。排除 10 个过晚捕获的样本、3 个终值或开盘参考不匹配的样本；24 个缺失有效条件特征的样本仍可用于资产级无条件基准，但不能用于条件模型预测。

新模型相对旧模型明显改善概率误差，但 BTC/ETH/SOL 都没有在选择集上同时击败市场概率和旧模型。本轮三个资产的 `selected` 均为 `null`，不能据此进入盈利交易验证或 canary。服务器下一批只用于被动观测与冻结模型的前瞻校验，不产生真实订单。

## 实现顺序与隔离范围

| 顺序 | 本轮实现 | 审计与限制 |
|---|---|---|
| 1 | 统一信号时刻、参考价观测时刻、到期时刻，以及 log-return 的 bps 单位 | 记录墙钟剩余时间和参考价有效期限；禁止把终值残差标准差当作 `bps/√s` 再乘 `√t` |
| 2 | 直接拟合 `10,000 × ln(最终结算参考价 / 信号参考价)` | 标签必须已闭市确认，开盘参考误差不超过 0.5 bps；按资产和检查点独立拟合 |
| 3 | 条件位置与尺度模型 | 使用 30/60 秒 TWAP 变化、Chainlink spot/TWAP 基差、参考价年龄、实际有效期限；高低活动度 × 高低期限四种尺度区间，并向资产总体方差收缩 |
| 4 | 比较条件 Gaussian 与经验残差 CDF | 固定两个 ridge 强度，分方向概率分箱；经验尾部平滑，缺失特征不补零，超出训练期限支持不预测 |
| 5 | 训练、选择、历史留出与延迟成交回放 | 60/20/20 按时间划分，同一 5 分钟窗口的各资产不拆分；只在选择集选模型，费用、滑点、部分成交和失败假设沿用原执行研究 |
| 6 | 冻结模型与独立新批次 | SHA-256 锁定模型；旧目录/不同模型禁止混写；新批次必须晚于历史训练数据并使用新的 run ID；核对在线记录和离线预测 |

新代码为 `src/digital_fair_value_terminal_model.rs` 和 `scripts/digital_fair_value_terminal_study.py`。只在 SDDFV 的 linked execution probe 中记录新模型预测。共享的 `late_resolution_winner_model.rs`、其他策略决策逻辑及其参数均未修改。

### 概率与时间定义

设当前信号参考价为 `C`、开盘价格为 `O`、最终参考价为 `F`：

```text
margin_bps = 10,000 × ln(C / O)
target_bps = 10,000 × ln(F / C)
effective_horizon_secs = (window_end_ms - reference_observed_at_ms) / 1,000
P(UP) = P(target_bps >= -margin_bps | signal_features)
```

因此预测的是最终 TWAP 相对当前 TWAP 的条件变化，不是 spot 价格的无条件随机游走。TWAP 30/60 秒锚点按实际观测间隔归一化；未来到达的 spot、过旧的参考价或不支持的期限不能用于条件预测。标称 30 秒批次中有效期限小于 22.5 秒的历史样本不会参与本轮拟合和评估。

## 本地结果

以下为条件经验残差模型 `conditional_empirical_ridge_10` 的**选择集**，Brier 越低越好。它只是固定候选之一，没有被选中。

| 资产 | 与市场成对样本 | 旧模型 Brier | 新模型 Brier | 市场 Brier | 模拟成交 | 扣费后模拟 PnL |
|---|---:|---:|---:|---:|---:|---:|
| BTC | 33 | 0.20160 | 0.08494 | 0.06835 | 5 | -$25.00 |
| ETH | 36 | 0.21958 | 0.13247 | 0.10200 | 8 | -$23.12 |
| SOL | 28 | 0.27714 | 0.14130 | 0.10635 | 14 | -$17.92 |

条件模型改善了旧模型，但仍弱于市场概率。市场成对样本远少于全部终值样本，不能把 3,622 个标签样本当成 3,622 次有效的市场优势验证。

部分历史留出场景出现正模拟 PnL，但该历史数据此前已被分析过，不能称为新的独立盈利证据，也不能按留出集的收益重新挑选模型。多个候选模型是互斥情景，不能把它们的 PnL 相加。

升阶段条件包括：选择集和新批次分别至少 50 个成对概率样本、至少 50 次延迟模拟成交、覆盖至少 5 个有成交的日期且多数日期正收益、扣费后 PnL 为正，并在成对 Brier 与 log loss 上击败市场概率。当前均未通过；即便以后通过，真实订单仍需另行批准和执行成本校准。

本轮完成的校验：完整 Rust 库测试 365 项通过；最终 SDDFV 相关测试 43 项通过；Python 校准/回放测试 26 项通过；3,622 个推断案例、18,010 次候选预测的 Rust/Python 最大概率差约 `6.97e-8`，小于 `1e-6` 阈值。已编译 release 可执行文件。

## 文件与配置

本地历史研究已生成，不能直接向该目录重复写入新的模型：

```text
results/short-duration-digital-fair-value/terminal-calibration-008-study-v3/
  model.json                冻结模型、训练来源、时间边界和固定执行假设
  report.json / report.md   概率、尺度、排除原因、每日损益和升阶段检查
  trades.ndjson             各模型、各时间分段的模拟成交
  inference-fixture.ndjson  无终值标签的推断输入，用于本地一致性测试
  parity-report.json        已通过的 Rust/Python 一致性证据
```

冻结模型 SHA-256：

```text
d7a54e5a9d16565041477a2fb632589d17f98733489eafafad93e0bc3c82e66b
```

本地 `.env` 已配置模型文件、SHA-256，并把 SDDFV 日志和结果切换到新批次：

```text
logs/short-duration-digital-fair-value/terminal-calibration-008-5m-30s/system.log
results/short-duration-digital-fair-value/terminal-calibration-008-5m-30s/events.ndjson
results/short-duration-digital-fair-value/terminal-calibration-008-5m-30s/report.json
results/short-duration-digital-fair-value/terminal-calibration-008-5m-30s/calibration-split-report.json
```

旧 `market-residual-execution-007` 数据保留，其他策略路径不变。保留 1 GiB 最低剩余磁盘空间限制；它不是整个目录只能占用 1 GiB 的配额。历史 Gaussian-floor 参数保持原值作为对照，`research_observation_only=true` 和 `research_execution_probe=true` 强制新模型不参与交易决策。

## 下一批运行与事后验证

本地复核一致性：

```bash
bash scripts/run_digital_fair_value_terminal_calibration.sh parity
```

本地运行新批次：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_terminal_calibration.sh dry-run
```

服务器把 binary 放在项目根目录时：

```bash
SDDFV_BINARY=./short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_terminal_calibration.sh dry-run
```

需要复制的最小文件：新 release 二进制、`.env`、上述模型目录中的 `model.json` 和 `parity-report.json`，以及 `scripts/run_digital_fair_value_terminal_calibration.sh`、`scripts/run_digital_fair_value_research.sh`。模型文件的目录结构需要保持一致。仅复制 `.env` 和二进制将因找不到模型而停止启动。无需复制 Rust 源码；推断 fixture 可留在本地。

执行脚本在启动前验证模型摘要和一致性报告，再进入只读观测模式。同一模型可在本批次停止后续跑；模型改变必须用新批次和新模型目录。**本次没有自动部署到服务器或启动采集。**

采集结束、数据同步到本地后：

```bash
# 原生整体运行报告：不会把候选模型的反事实 PnL 当成实际交易。
bash scripts/run_digital_fair_value_terminal_calibration.sh report

# 刷新新批次的终值标签 reference-audit.json。
bash scripts/run_digital_fair_value_terminal_calibration.sh calibrate

# 以已冻结模型评估新批次，不重新训练；包括候选概率与延迟模拟 PnL。
bash scripts/run_digital_fair_value_terminal_calibration.sh validate-frozen
```

`validate-frozen` 默认输出 `results/short-duration-digital-fair-value/terminal-calibration-008-fresh-study/`。后续同步更多数据重新分析时，使用新的 `SDDFV_TERMINAL_VALIDATION_OUTPUT`，不要覆盖已冻结证据。Python 研究脚本依赖同目录的 `digital_fair_value_residual_study.py`，在本地分析无需把它们部署到服务器。

如要重新训练研究模型，显式指定新的 `SDDFV_TERMINAL_MODEL_DIR`，运行 `study` 后再做 `parity`；新模型也必须匹配新的采集批次。当前三个资产均未选出模型，下一批价值是验证条件分布稳定性，不能承诺采集更多数据就会变为可盈利策略。
