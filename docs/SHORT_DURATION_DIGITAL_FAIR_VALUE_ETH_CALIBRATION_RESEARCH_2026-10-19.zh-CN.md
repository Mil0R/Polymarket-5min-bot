# ETH 概率校准离线研究：实现与首轮结果（2026-10-19）

**离线研究工具已实现并完成完整数据跑数；首轮没有得到可交易的概率修正方案。** 逻辑校准未改善评分；市场收缩改善原成交子集上的概率评分，但最终模型权重为 0，完全使用市场概率，重新选单后没有信号或成交。candidate.json 是市场基准研究工件，不是交易部署候选。

本次未修改 Rust 在线推断、冻结基础模型、运行参数、资金限制或服务器进程，未启动新观察任务。没有网络请求或真实订单。已有全部数据作为开发资料，滚动结果也不称为新的独立验证。

## 实现范围

新增 [离线工具](../scripts/digital_fair_value_probability_calibration.py) 与 [测试](../scripts/test_digital_fair_value_probability_calibration.py)。从已有终值回放提取 causal_signal_policy，复用同一套因果开盘与费用校验；旧回放行为保持一致，相关测试与归档成交逐笔核对通过。

固定研究对象为 ETH、五分钟窗口、到期前 30 秒、原 terminal_empirical_baseline。基础模型 SHA256 为 09460671790e82e45a8e3512b4f6817d56bfd0376ce18bacabc77f6aac486af6。基础模型开发期与运行 ID 的重叠检查通过；没有重新拟合基础模型或同时调 λ=10。

- 训练资料包括信号时合格且有新鲜市场中间价的窗口，包含无 edge 和未成交窗口；不按盈亏挑训练数据。
- 三个共同支持情景：原概率对照、正则化逻辑校准、市场概率收缩。另保留原模型全覆盖对照，检查覆盖或弃权是否隐藏损失。
- 信号资格不读取延迟报价或最终胜负。拟合只使用原始 UP 概率、训练胜负；市场收缩另外读取信号时市场 UP 概率。方向、动量、开盘距离、延迟成交与 PnL 不作拟合特征。
- DOWN 概率始终为 1−UP 概率。同一市场窗口与原始所选方向用于配对评分；校准后重新选单的结果另算。
- UTC 日初拟合过去数据，只用 verified_at 不晚于日初的标签，终值另加 5 分钟隔离。每次训练至少 100 条、两类各至少 10 条、至少 3 个不同日期。
- 重新选择方向、检查信号及延迟 edge，重跑费用、深度、失败率、有限资金、结算释放、回撤和压力情景。
- 保存模型、代码、输入、标签、原诊断队列及输出哈希；拒绝覆盖已有目录，验证过程中输入不变。

verified_at 可能晚于标签真正首次可用时间。这里按缓存记录做保守限制，不意味着更早绝无可能获得标签。

## 样本与时间覆盖

支持的 ETH 基础预测窗口 3,952；共同支持、可拟合窗口 356。大量其它窗口的最优 ask 不在原价格范围，不混入可选单区间夸大评分。

严格按标签可用性与训练数量，首个评估日为 UTC 10-11，最终评估到 10-18 截止。滚动评估 2,154 个支持窗口，其中共同支持 215 个；完整评估日为 UTC 10-11 至 10-17 共 7 日。10-18 为部分日，仍计入总评分与盈亏。

原终期独立诊断的 143 笔成交中，114 笔可在本轮滚动区间评估，29 笔属于预热阶段。没有用全样本拟合补出预热日预测。原概率共同支持与全覆盖对照均为 148 个 edge 信号、114 笔成交，P&L 完全一致，覆盖限制没有减少这段原始成交。

114 笔原始方向、份额、费用、成本、预测概率与 PnL 均与归档诊断逐笔一致。本轮原概率 +34.49 只属于这些 114 笔，不是完整 143 笔的结果。完整 143 笔为 -7.96，被预热排除的 29 笔约 -42.45，不能混用。

## 两个简单方案

逻辑校准：p_cal = sigmoid(a × logit(p_raw) + b)。固定向恒等变换 (a=1,b=0) 正则，强度 1，a 限制在 [0.05,5]、b 在 [-5,5]，没有搜索超参数以追求盈利。最终全开发资料拟合 a=0.939144、b=-0.022609，训练 356 条。历史评分使用各日只见过去标签的参数，不用最终全量参数回填历史。

市场收缩：p_cal = market + alpha × (p_raw − market)，alpha 限定 [0,1]，按训练集 Brier 的解析最优值求解。滚动 alpha 为 0 至约 0.0659，预测主要依赖市场；最终全开发资料 alpha=0。candidate.json 明确标为 market_reference_only，online_trading_enabled=false。

## 同一原始成交子集的评分

固定同一 114 笔、同一合约方向和胜负，不使用校准器自行选出的有利子集。

| 方法 | Brier | 平均概率偏差 | Brier 差：校准−原始 | 完整日差值区间 |
|---|---:|---:|---:|---|
| 原概率 | 0.19444 | +20.74 个百分点 | +0.00000 | [+0.00000,+0.00000] |
| 逻辑校准 | 0.19615 | +20.35 个百分点 | +0.00171 | [-0.00122,+0.00607] |
| 市场收缩 | 0.14100 | +1.31 个百分点 | -0.05345 | [-0.09037,-0.00528] |

同样本纯市场 Brier 为 0.14005。逻辑校准 Brier 略恶化，改善区间跨零；市场收缩的改善区间为负，但其 0.14100 仍未优于纯市场。结果支持“当前模型偏离市场没有显示稳定增量优势”的研究线索，不证明模型信息在所有状态下永久无用。

区间采用 UTC 日块配对 bootstrap，4,000 次、固定 seed=20261019，保留有覆盖的零成交日。仅 7 个完整评估日，资料全部已查看，区间为开发分析，不是前瞻验收。Brier 同时反映校准和区分能力，须连同可靠性分箱与 log loss 解读。

## 重新选单与回放

| 方法 | edge 信号 | 成交 | 净 PnL | 压力成交 | 压力 PnL |
|---|---:|---:|---:|---:|---:|
| 原概率 | 148 | 114 | +34.49 | 109 | -40.67 |
| 逻辑校准 | 142 | 112 | -13.80 | 108 | -71.22 |
| 市场收缩 | 0 | 0 | +0.00 | 0 | +0.00 |

逻辑校准没有修复原成交概率评分，重新选单也未改善该区间收益。市场收缩无 edge、无成交；P&L 为 0 是没有交易的算术结果，不是稳定盈利或低风险证据。

概率研究筛选将 market_shrink 列为评分改善参考；策略检查同时记录 executable_strategy_found=false，原因包含零/不足成交、净 PnL 和压力 PnL 非正、日块收益区间不为正、最终参数等价于市场。没有可启动的交易修正候选。

## 下一轮草案与结论

prospective-protocol.json 为 draft_not_started，launch_ready=false，开始/结束时间为空，当前阻止启动。草案要求先冻结一个实际方案及哈希，固定未来日期、样本数量及实际改善目标；主指标为同一原始选单成交的校准 Brier 减原始 Brier，辅助检查可靠性、log loss、市场比较、覆盖、收益与压力结果。旧的 100 笔/5 天诊断门槛不复用为修正模型验收。

工件保留基于仅 7 个日块、目标 Brier 改善 0.02 的粗略样本规划：约 88 个完整日、1,144 笔、约 90 天。这是高不确定性的规划敏感性结果，没有采用为新任务期限，不能视为功效保证。当前市场基准退化方案不应据此启动长期观察。

下一步研究：

1. 模型相对新鲜市场概率，在哪些预先限定环境下还存在可重复的增量信息。
2. 全局 UP 概率校准与选单后的条件偏差为何不同；只做全局映射是否不足。
3. 保持候选数量有限，固定开发规则，再用未查看新数据验证。当前负面结果保留，不堆参数直到回测盈利。

本次完成离线实现、完整跑数、归档核对和未启动草案，没有声称概率问题已经解决。

## 复现与检查

从仓库根目录运行，输出目录必须是新的：

~~~bash
python3 scripts/digital_fair_value_probability_calibration.py \
  --input results/short-duration-digital-fair-value/analysis-011-20261019T013308Z/source/events.ndjson \
  --fixed-cohort results/short-duration-digital-fair-value/analysis-011-20261019T013308Z/prospective-eth-review/episodes.ndjson \
  --output results/short-duration-digital-fair-value/eth-probability-calibration-012-review
~~~

~~~bash
python3 -m unittest discover -s scripts -p 'test_digital_fair_value*py'
~~~

65 项相关测试通过，包括未来标签隔离、标签审计时间、信号资格不读取延迟信息、概率一致性、重新选单方向变化、固定方向配对评分、零成交日保留、市场退化方案阻止启动。实际 114 笔基准逐笔核对通过；输入与冻结模型哈希不变。工作区其它策略的已有修改保持原样。

## 工件

- [完整结果](../results/short-duration-digital-fair-value/eth-probability-calibration-012-20261019T015813Z/report.json)
- [最终参数](../results/short-duration-digital-fair-value/eth-probability-calibration-012-20261019T015813Z/fitted-calibrators.json)
- [市场参考工件](../results/short-duration-digital-fair-value/eth-probability-calibration-012-20261019T015813Z/candidate.json)
- [未启动协议草案](../results/short-duration-digital-fair-value/eth-probability-calibration-012-20261019T015813Z/prospective-protocol.json)
- [开发资料](../results/short-duration-digital-fair-value/eth-probability-calibration-012-20261019T015813Z/dataset.ndjson)
- [滚动预测](../results/short-duration-digital-fair-value/eth-probability-calibration-012-20261019T015813Z/forecasts.ndjson)
- [基础与压力成交](../results/short-duration-digital-fair-value/eth-probability-calibration-012-20261019T015813Z/trades.ndjson)
- [归档核对](../results/short-duration-digital-fair-value/eth-probability-calibration-012-20261019T015813Z/verification.json)
- [数据与代码哈希](../results/short-duration-digital-fair-value/eth-probability-calibration-012-20261019T015813Z/manifest.json)
