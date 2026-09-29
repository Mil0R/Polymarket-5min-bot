# Short Duration Digital Fair Value：Run 004 修复说明

`research-model-audit-003` 的 24 笔模拟成交暴露了两个独立问题：服务器磁盘写满导致 NDJSON 尾行损坏；Gaussian 模型平均预测胜率约 52.6%，实际胜率仅 12.5%，且市场成交中点的 Brier 明显更好。因此 Run 004 仍是只读研究批次，不允许提升为 canary。

## 本轮修复

1. 事件日志在序列化完整 JSON 后才执行一次行写入；任何写入失败都会使 journal fail-closed，禁止继续向半行后追加。
2. 启动时检查最后一个字节；如果上一轮因磁盘写满留下不完整尾行，只截断该尾行，不重写历史完整事件。
3. 默认保留至少 1 GiB 可用空间，并限制单批事件文件最大 2 GiB。触发任一限制后安全停止，避免再次写满磁盘。
4. 报告器遇到旧版“半行 + 新事件”时可恢复有效后缀，但会记录 `malformed_journal_lines` 并令完整性门禁失败，绝不静默晋级。
5. 参考价格时延拆分为：
   - `observation_age_ms`：观测时间到决策时间；
   - `upstream_publish_delay_ms`：观测到上游发布时间；
   - `network_receive_latency_ms`：上游发布到本机接收；
   - `local_receive_age_ms`：本机接收到决策时间。
6. execution schema 升为 v6；准入质量同时要求观测年龄、网络接收延迟和本地接收年龄合格。v5 及更早数据保留在总账，但只能作为 research-only。
7. 报告新增各资产波动率下限命中率、平均/最大观测波动率，以及同一批成交上的概率模型对照；每次执行额外记录 `0.25 / 0.50 / 1.00` 三档资产专属 floor scale，供下一轮按样本选择，正式决策参数暂不盲目修改。
8. 新增 `market_anchored_v1` 研究模型：将冻结模型概率按 75% 权重收缩到可执行盘口中点。它只写入研究审计字段，不参与信号、成交或收益门禁。
9. 高频盘口研究快照默认从 1 秒降为 2 秒，保留行情源逐秒记录，同时降低事件文件增长速度。

## 运行

服务器必须先替换 release 二进制、两个运行脚本和冻结模型文件，然后确认磁盘剩余空间超过 1 GiB：

```bash
df -h /home/ubuntu/poly

SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_model_audit.sh dry-run research-model-audit-004
```

生成报告：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_model_audit.sh report research-model-audit-004
```

Run 004 的路径为：

- `logs/short-duration-digital-fair-value/research-model-audit-004-5m-30s/system.log`
- `results/short-duration-digital-fair-value/research-model-audit-004-5m-30s/events.ndjson`
- `results/short-duration-digital-fair-value/research-model-audit-004-5m-30s/report.json`

## 验证门槛

先运行 48–72 小时。下一轮分析重点不是总 PnL，而是：v6 合格样本数量、分资产 Brier、市场中点与 `market_anchored_v1` 的对照、参考流 gap、波动率下限命中率和 journal 完整性。Gaussian 与冻结 empirical 模型若继续显著差于市场中点，应停止原模型成交验证，转入以市场隐含概率为先验的 walk-forward 模型开发。
