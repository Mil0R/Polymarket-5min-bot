# Short Duration Digital Fair Value：Run 005 验证计划

Run 004 的 18 笔 v6 合格模拟成交净收益为 -31.32 USDC，Gaussian Brier 为 0.293；相同成交上的市场中点 Brier 为 0.120。完整市场观测中的 Gaussian 校准较好，但在具备可执行盘口的配对样本中，市场中点明显优于模型。因此 Run 005 不修改实盘或 canary 准入，只验证市场相对优势和三项保护条件。

## 新增研究证据

每笔 Gaussian 已选中且完成延迟后 REST 模拟成交的样本，额外记录以下严格子集：

1. `market_anchored_edge`：75% 市场中点 + 25% 冻结模型的概率，在相同成交成本下仍满足最低净优势。
2. `entry_price_floor`：实际模拟成交均价不低于 0.30。
3. `reference_direction_agreement`：所买方向与当前参考价相对开盘价的方向一致。
4. `entry_and_direction`：同时满足第 2、3 项。
5. `market_anchored_entry_and_direction`：同时满足市场锚定净优势、价格下限和方向一致。

这些组复用父成交的延迟后 REST 盘口、深度、部分成交、滑点、费用和最终结算，不会产生更优惠的成交假设。它们是 Gaussian 已选成交的同方向严格子集，不是独立策略回放，也不参与下单或 canary 晋级。

校准报告和时间留出报告现在同时统计冻结研究模型与 `market_anchored_v1`，并保留同一可交易样本上的市场中点对照。

## 配置

```env
SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MARKET_PRIOR_WEIGHT=0.75
SHORT_DURATION_DIGITAL_FAIR_VALUE_RESEARCH_MIN_ENTRY_PRICE=0.30
SHORT_DURATION_DIGITAL_FAIR_VALUE_JOURNAL_MIN_FREE_BYTES=1073741824
```

这些参数均为研究参数，不改变当前 Gaussian 决策路径。

## 运行

上传新二进制、`.env`、`scripts/run_digital_fair_value_model_audit.sh` 和 `scripts/run_digital_fair_value_research.sh` 后：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_model_audit.sh dry-run research-model-audit-005
```

建议运行 36–48 小时。停止并同步数据后生成三份报告：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_model_audit.sh report research-model-audit-005

SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_model_audit.sh calibrate research-model-audit-005

SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_model_audit.sh validate research-model-audit-005
```

## 继续研究门槛

- 三种资产各至少 50 个带可执行市场中点的独立市场样本；
- 只比较相同样本，候选模型 Brier 必须低于市场中点；
- 至少一个预先定义的反事实组在保守执行成本后保持正 PnL；
- train、validation、holdout 方向一致，且不是仅由少数低价合约贡献；
- journal 完整性通过，旧 Gaussian 总账继续保持 research-only。

未达到以上条件时，不进入 canary，也不通过降低费用、滑点或延迟假设改善结果。
