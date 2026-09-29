# SDDFV：市场残差与延迟成交验证（Run 007）

本轮实现的是可执行成本证据采集、离线残差拟合和冻结验证工具，不是恢复交易。
`.env` 仍为 observation-only；不会提交真实订单，也不会在线产生 shadow fill。
不能将本轮实现或旧数据中较小的预测改善解读为已经找到正期望套利。

## 已完成的改进

- 原生 `validate` 报告新增 `paired_scores`：每个候选与市场基线使用完全相同的有效样本。
  保留原来的全样本分数用于诊断，不能将全样本模型分数与缺失样本较多的市场分数直接比较。
- 每市场最多一次、30 秒 checkpoint，获取 UP/DOWN 信号 REST 盘口；等待至少 1 秒后再次并行获取 UP/DOWN。
  用 episode_id 关联信号和延迟盘口，记录实际墙钟/单调时钟延迟、完整请求耗时、身份和时间戳校验、结算窗口。
  probe 模式不订阅盘口 WebSocket，避免盘口重连阻塞 REST 采样；参考价格仍使用 RTDS WebSocket。
  ask/bid 分别排序后最多保留 20 档，并记录原始档数；更深档位不补齐，避免乐观估计。
- 不再要求两边都有 bid 才能保存研究证据。优先使用 UP 中点，其次 DOWN 中点的补值。
  都没有中点时，单独记录 `[1 - DOWN ask, UP ask]` 区间。
  区间中心不是实测中点；区间样本不进入主要预测比较及晋级门槛。
- 新增按 BTC/ETH/SOL 独立拟合的 offset-logistic 市场残差模型。
  公式：`p = sigmoid(logit(market_probability) + beta · features)`，概率修正最多 ±0.10。
  固定特征为截距、Gaussian−市场概率、信号时刻方向性价差（bps）。不使用未来盘口或最终结算价格作为特征。
  标准化仅使用训练集；正则候选 1/10/100 仅由验证集选择，Brier 和 log-loss 都改善才保留候选。
  不合格资产回退到零残差市场基线。原来的 75% 市场/25% 模型混合仅为旧诊断，不参与新模型选择和成本回放。
- 时间切分按市场开盘时间统一进行，同一窗口的资产不会跨分区。训练/验证/留出比例 60/20/20。
  不用留出集调参或重训；导出模型、参数、策略假设、时间边界和输入 SHA256。
- 离线逐档 ask 回放：$5 总预算（含费用）、10 bps 滑点、75% 深度、至少 25% 可成交预算、2% 确定性失败模拟。
  延迟后仍需净边际 ≥0.03；没有 bid 不是买入失败原因，但没有有效 ask/depth 就不能成交。
  实际延迟超出 1～2 秒、过期、盘口超过 1 秒、REST 超过 2 秒、参考价/传输超限或证据不全，分组排除。
  概率固定为信号时刻输出，不在延迟后重新计算；收益仅用已确认 closed/terminal 的最终标签。
  `settlement_pnl` 是模拟持有到结算的成本后收益，不是账户真实收益。
- 原生 `report` 增加 probe episode 数、有效 delayed ask 侧数、基线来源和盘口错误计数。
  这些数量不是成交数；净收益查看离线 residual study 报告。

## 旧 Run 006 的本地复核

结果目录：`results/short-duration-digital-fair-value/market-residual-observation-006-residual-study/`。
共 424 个带真实中点且结算确认的去重样本；按有完整特征的市场时间重新切分，
所以切分边界与原生全观察样本切分不同，不应直接比较两个切分的分数。

| 资产 | 留出样本 | 市场 Brier | 残差 Brier | 结论 |
| --- | ---: | ---: | ---: | --- |
| BTC | 32 | 0.017621 | 0.017179 | 微小改善，样本不足 |
| ETH | 27 | 0.105654 | 0.105654 | 验证集无双指标改善，回退市场基线 |
| SOL | 23 | 0.035411 | 0.035411 | 验证样本不足，未选择残差模型 |

旧批次没有逐市场信号关联的 1 秒后盘口。所有旧样本执行验证标为
`missing_linked_delayed_book`，没有虚构成交或收益；三个资产的门槛均未通过。

## 服务器运行

停止旧进程后上传新 release 二进制、`.env` 和以下脚本：

- `scripts/run_digital_fair_value_market_residual_observation.sh`
- `scripts/run_digital_fair_value_research.sh`
- `scripts/digital_fair_value_residual_study.py`（也可仅在本地分析时使用，标准库，无额外 Python 依赖）

旧 empirical model.json 是可选诊断依赖；缺失时新采样仍可运行。
存在但 SHA 不匹配时脚本拒绝加载，不会静默用另一个模型。

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_market_residual_observation.sh dry-run
```

如果服务器二进制放在项目根目录，改为 `SDDFV_BINARY=./short_duration_digital_fair_value`。
直接使用 `.env` 启动 `./target/release/short_duration_digital_fair_value dry-run` 也会进入相同的只读采样模式。

新路径：

- `logs/short-duration-digital-fair-value/market-residual-execution-007-5m-30s/system.log`
- `results/short-duration-digital-fair-value/market-residual-execution-007-5m-30s/events.ndjson`

系统日志优先配置和策略日志配置已统一到相同新目录，旧 Run 006 不覆盖。
重启同一批次会追加；模型/假设改变时使用新批次名。
保留至少 1 GiB 空闲空间的保护，journal 上限仍为 2 GiB，没有更改用户此前的磁盘保护设置。

先运行 48～72 小时检查采样覆盖、错误分组和延迟分布，不把这个时长当作盈利验收标准。
同步数据到本地后生成报告：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_market_residual_observation.sh report
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_market_residual_observation.sh calibrate
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_market_residual_observation.sh validate
bash scripts/run_digital_fair_value_market_residual_observation.sh study
```

`calibrate` 查询 Gamma 已结算标签并缓存，可重复运行更新仍待结算的市场。
`validate` 默认只读缓存，不联网。`study` 导出 `model.json/report.json/report.md/trades.ndjson` 到
`results/short-duration-digital-fair-value/market-residual-execution-007-residual-study/`。
trade 文件中的记录均是模拟成交。若数据追加后需要重新拟合，必须指定新的输出目录，
避免覆盖已冻结模型：`SDDFV_RESIDUAL_STUDY_OUTPUT=results/short-duration-digital-fair-value/study-007-v2 bash ... study`。

## 冻结后的下一轮

停止 Run 007，明确选定一份 model.json 后，启动全新更晚批次，例如：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_market_residual_observation.sh dry-run market-residual-execution-008
# 数据同步后，先对 008 执行 report/calibrate/validate，再执行冻结模型评估：
SDDFV_RESIDUAL_FROZEN_MODEL=results/short-duration-digital-fair-value/market-residual-execution-007-residual-study/model.json \
  bash scripts/run_digital_fair_value_market_residual_observation.sh study market-residual-execution-008
```

`--model` 不拟合、不选新参数，并拒绝训练数据时间重叠、复用训练 journal 或不同 checkpoint。
报告记录冻结模型 hash，研究模型不会由 binary 自动加载用于下单。

目前预设的研究门槛（保守的实验规则，不是盈利保证）：每资产训练 ≥75、验证 ≥25、留出/冻结新批次 ≥50 个预测样本；
验证/留出或独立新批次均需要 ≥50 笔有效延迟模拟成交、净收益为正且优于市场基线；
至少 5 个有成交的 UTC 日期块、至少 60% 的日期块为正。
训练研究报告还必须进入全新冻结批次，不能仅凭一次留出结果晋级。
因此 48～72 小时仅够初步数据质量检查；收益验收时长取决于有效独立成交速度。
通过工具门槛后也不自动恢复 shadow/实盘：仍应复核各市场真实费率、成交可达性和跨日稳健性，
再另行实现/启用在线残差决策路径。当前研究工具有意保持 `online_trading_enabled=false`。
