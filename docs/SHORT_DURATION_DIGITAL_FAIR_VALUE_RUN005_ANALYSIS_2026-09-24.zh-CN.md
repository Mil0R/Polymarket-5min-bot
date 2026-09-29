# Short Duration Digital Fair Value：Run 005 结论与 Run 006 调整

## 结论

停止按 Run 005 参数继续运行，也不进入 canary。Run 005 已经否定“Gaussian/frozen probability 相对盘口的偏差可以直接作为买入优势”这一版本的交易假设；下一轮只进行市场残差研究，不再模拟旧模型订单。

所有收益均为 shadow 模拟值，不是账户实际盈亏。

## Run 005 证据

- 运行约 55 小时，覆盖两个独立进程段；90 笔 v6 合格模拟成交均已结算，无重复、悬挂或超预算记录。
- 投入模拟资金 450 USDC，净收益 -224.2842 USDC，ROI -49.84%，胜率 12.22%。
- BTC：53 笔，ROI -27.91%；ETH：30 笔，ROI -88.05%；SOL：7 笔，ROI -52.18%。
- 表现最好的预注册反事实组 `market_anchored_edge` 仍为 49 笔、ROI -19.71%；其余四组 ROI 为 -29.97% 至 -37.37%。
- 90 笔交易样本中，Gaussian Brier 为 0.23487，市场中点为 0.09571。
- 1,653 个结算校准观测全部取得标签；在具有相同盘口的 258 个配对样本上：
  - 市场中点 Brier：0.08779；
  - market-anchored Brier：0.09472；
  - Gaussian Brier：0.20008；
  - frozen research model Brier：0.20029。
- 全量样本上 frozen model Brier 为 0.05189，但这主要说明它能识别已经明显偏离开盘价的容易样本；在可交易分歧样本上显著落后于市场，不能转化为交易优势。
- 盘口时间戳、参考价格和 REST 延迟质量门通过。WS 与 REST 样本均为负，因此亏损不能归因于单一数据源或网络延迟。

## Run 006 调整

新增 `market-residual-observation-006` 被动观测模式：

1. `RESEARCH_OBSERVATION_ONLY=true` 时不产生 opportunity、shadow fill 或模拟 PnL。
2. 每个市场在 30 秒检查点记录模型和盘口配对观测；WS 不完整时只做一次 REST 补采，禁止每秒重复回退。
3. 校准器对同一市场/检查点只保留一个样本；仅当第一条缺少盘口中点时，才允许 REST 配对记录替换它。
4. 原始 RTDS tick 默认不再逐条写入；盘口研究快照降为每 10 秒一次，以控制磁盘增长。
5. 旧 Gaussian、frozen 和 anchored 模型只作为比较基线，不再用于订单准入。

## 运行与验收

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_market_residual_observation.sh dry-run
```

建议先运行 48 小时。停止后：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_market_residual_observation.sh report

SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_market_residual_observation.sh calibrate

SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_market_residual_observation.sh validate
```

Run 006 的目的只是扩大严格配对的盘口样本，建立市场中点基线。只有新的残差模型在时间留出集上同时优于市场中点，并经独立延迟盘口回放得到正收益后，才重新开放 shadow fill；否则终止该策略研究。
