# SDDFV 010：事前开盘证据与观察模式审计

2026-10-04。适用 BTC/ETH、5 分钟市场、30 秒检查点。修复采集与报告，不是重新训练或盈利验证通过。

## 改动与边界

1. 官方开盘查询优先使用 Gamma 完整 `GET /events/slug/{slug}`，缺证时回退 `GET /events?slug=...`；每个请求限时 1.5 秒。官方详情接口依据：[Polymarket 文档](https://docs.polymarket.com/api-reference/events/get-event-by-slug)。此接口存在不代表它在每个实时市场都提供 priceToBeat。
2. 每市场每 60 秒提前采集一次，检查点再次并行查询开盘与费用。保存来源、HTTP 状态、接收时间、每次回退原因及缓存首次验证时间。事件/市场 slug、condition ID、实时状态及窗口时间必须吻合；只保存 priceToBeat，不复制 finalPrice/胜负。
3. 开盘缓存最多 128 个市场，保留原始接收时间，不以缓存读取时间刷新证据。原有信号前 10 秒时限不变；开盘值发生冲突后不再静默恢复为可信证据。接口仍未提供字段就继续标记缺证，不用事后结算标签修补主门槛。
4. 仅 SDDFV 改为 TWAP 与研究现货两条独立 WebSocket。单条流 stale 只重连自己的连接，保留已有历史；记录连接、断开与失败原因。TWAP 陈旧检测时限与交易参考价年龄门槛不变。其他策略仍调用原有订阅和重连路径。
5. 每源每秒最多保留两个接收版本，避免检查点后的同秒更新抹掉已经知道的旧版本。增加 60 秒覆盖率、缺秒位置、源/接收年龄及已知终值窗口成分审计；54/60 的代理重建门槛不变，不插值、不补零、不纳入未来价格。
6. Native report 增加运行模式与 capture_audit。Python 回放区分 observation_only、固定模型但运行阶段未验证、真正的 freeze/start；仅提供模型文件不构成冻结证据。观察模式强制禁止盈利升级，缺中间价与缺条件特征分别统计，官方终值暂缺不再混称开盘偏移。

模型文件、SHA-256、概率公式、资金设置、滑点、费用、1 秒执行延迟及价格/边际门槛均未改变。Gaussian 波动率下限统计仍只是对照，不能视作 v2 决策模型。

## 新目录与版本隔离

采集版本 `audit_capture_version=2`。新二进制和脚本拒绝向旧版本 run-plan/事件目录追加采集数据。已有 009 原始数据保持不动；分析旧文件不要求新版采集标记。

`.env` 和运行脚本默认批次已改为 `market-offset-010`：

```text
logs/short-duration-digital-fair-value/market-offset-010-5m-30s/system.log
results/short-duration-digital-fair-value/market-offset-010-5m-30s/events.ndjson
results/short-duration-digital-fair-value/market-offset-010-5m-30s/run-plan.json
results/short-duration-digital-fair-value/market-offset-010-5m-30s/report.json
results/short-duration-digital-fair-value/market-offset-010-5m-30s/calibration-split-report.json
```

继续使用未重新拟合的 `market-offset-009-development-v2/model.json`、protocol.json、parity-report.json 和 report.json。改变采集批次不等于更换概率模型。

## 运行与验收

```bash
cd /home/ubuntu/poly
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_next_round.sh observe market-offset-010
```

先运行 2–4 小时验收采集，不运行 `start`，不直接绕过 staged runner 调用 v2 dry-run。初次创建固定截止时间，重启不能延长到盈利；该模式不提交真实订单。

验收重点：

- 确认两条连接已订阅、分流重连原因可追踪，缺秒准确分组而不隐藏。
- 确认 opening_reference_observation 同时包含 prefetch/checkpoint；若 priceToBeat 仍缺失，报告明确字段缺失或 HTTP 状态，而非含糊的模型失败。
- 有官方开盘字段的样本须与本地开盘价吻合，原接收时间在信号前 10 秒内、源身份一致、实时窗口尚未关闭；仍然缺证则不能开始 100 笔盈利验收。
- 观察 mode、采集版本与模型 hash 正确，旧 009 不被追加。单边盘口不伪造 midpoint。

同步后在本地分析：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_next_round.sh report market-offset-010
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_next_round.sh calibrate market-offset-010
bash scripts/run_digital_fair_value_next_round.sh validate market-offset-010
```

最后一条生成带时间后缀的固定模型观察回放，不重选候选，也不宣称是独立冻结盈利验证。

## 部署文件

需更新 `.env`、`target/release/short_duration_digital_fair_value`、两个运行 shell 脚本，以及这些 Python 运行时模块：

```text
scripts/digital_fair_value_next_round.py
scripts/digital_fair_value_terminal_study.py
scripts/digital_fair_value_residual_study.py
scripts/digital_fair_value_risk.py
scripts/digital_fair_value_observation_audit.py
```

第五个 Python 模块是新增依赖，不能漏传。服务器已有的 009 开发模型工件不需改写。若为全新服务器，仍需复制那套模型、协议、parity-report.json 和 report.json；运行 parity 另需 inference-fixture.ndjson。无需 Rust 源码或服务器编译。

本次没有自动上传或启动服务器进程。真实接口是否提供实时开盘字段，以及分流连接是否改善实际缺秒，需要新一轮服务器采集验证，不能用合成测试代替。

## 本地验证记录

- Rust 库测试 376 项、Python SDDFV 测试 45 项通过；shell 语法与 diff 空白检查通过。
- release：`target/release/short_duration_digital_fair_value`；SHA-256 为 `3a7e0233ec72aa0ac842101833cb47a4bd13c01c921b01bb7f8fddc89b580d83`。
- v2：3,940 个输入案例、5,145 次预测，一致性通过，最大差 `2.22e-16`。
- v1 回归工件：2,286 个案例、11,342 次预测，一致性通过，最大差约 `6.97e-8`。
- 原始 009 批次：63 次可比较在线/离线预测误差为零；模型和原始事件文件哈希未改变。
- 修正版报告正确标记 `observation_only_fixed_model_replay`，官方终值暂缺单列为 3 个，阶段 4 为 `not_started_observe_only`。
- 修正版报告工件位于 `results/short-duration-digital-fair-value/market-offset-009-capture-fix-audit-2026-10-04/`。这些是旧数据的复核报告，不是 010 新采集结果。
