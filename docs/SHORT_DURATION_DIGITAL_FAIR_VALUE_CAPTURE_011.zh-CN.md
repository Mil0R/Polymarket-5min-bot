# SDDFV 011：开盘来源验证与缺失样本采集修复

本轮针对 `market-offset-010` 的采集缺陷，不重新拟合模型，不放宽盈利门禁，不提交真实订单。旧批次日志和结果保留不动。

## 修复内容

1. **独立开盘来源。** Gamma 的身份匹配响应缺少 `eventMetadata.priceToBeat` 时，不再重复请求等价的 Gamma list 路由；改为尝试 Polymarket 官方事件页面的服务端 JSON 查询。只有币种、5 分钟起止时间、condition ID、事件状态、Chainlink 60 秒 TWAP 规则和 query key 全部匹配，才接受 live `openPrice`。终值、未来时间、冲突价格、页面结构变化均拒绝。保留数据更新时间、接收时间、HTTP 状态、响应长度和 SHA256，不保存 HTML。
2. **被动采集与模型就绪解耦。** 缺开盘样本、参考流陈旧等情况仍保存 signal/delayed 两次 REST 盘口、深度、时间与来源证据。`signal.model_status=unavailable`，预测、开盘和模型字段为 null，并记录具体原因；这些记录不能进入概率或盈利统计。采集总期限 20 秒，超时/保存失败显式记录。
3. **检查点对账。** 新增 `checkpoint_capture` 的 planned/started/captured/failed 记录。报告用文件时间水位区分已捕获、明确失败、漏采/中断、进行中及尚未到期；不把同步文件尾部或旧批次缺少计划记录误判为漏采。
4. **连接诊断。** 独立 TWAP/现货连接分别记录首次有效数据，每 60 秒记录连接内的更新静默、PING/PONG 数量与最后 PONG 年龄。未订阅的流静默值为 null；现货连接不会再显示虚假的 TWAP 长时间断流。订阅发送成功不等于收到订阅确认或有效数据。其他策略的交易模型、订阅和重试阈值未调整。
5. **盘口分组。** 报告按资产、signal/delayed、单边/双边/空/无效盘口分组。不从两个 ask 或单边报价捏造 midpoint。全部样本无法建模时仍生成诊断报告，收益状态为不可评估，不是“零收益验证”。

官方页面是前端来源，不是已承诺稳定的 API。严格身份检查只证明来源对应当前窗口，仍需用新采集数据确认稳定性和与本地 opening TWAP 的一致性；不能用旧窗口事后补取的开盘价修复历史信号。

页面传输全程限制 3 秒、4 MiB。本地实测完整页面约 1.2 MiB、1.86–2.09 秒，原 1.5 秒页面期限不足；这项采集期限调整不改变信号的 10 秒证据年龄和 0.5 bps 误差门禁。慢请求依旧明确超时并拒绝采用。

## 保持不变

- 固定模型：`results/short-duration-digital-fair-value/market-offset-009-development-v2/model.json`。
- 模型 SHA256：`09460671790e82e45a8e3512b4f6817d56bfd0376ce18bacabc77f6aac486af6`。
- BTC/ETH、5m、30s 检查点、1s 延迟、模型政策及成本压力假设不变。
- 虚拟预算 $1000、单笔 $5、总持仓成本上限 $20、单资产 $10 不变。
- 开盘证据接收时间至信号不超过 10 秒、价格误差不超过 0.5 bps 的原门禁不变。缓存不能把旧接收时间改成新时间。
- 磁盘剩余空间门禁仍为 1 GiB。新增开盘审计只存小型证据，不存约 MB 级页面；网络下载量会高于原 Gamma-only 采集。
- `freeze`/`start` 仍受未通过的本地门禁阻挡，011 只能用于 observe 采集验收。

## 新目录

`.env` 和启动脚本默认批次已改为 `market-offset-011`，采集版本为 3。不要继续向 010 追加修复版本的数据。

```text
logs/short-duration-digital-fair-value/market-offset-011-5m-30s/system.log
results/short-duration-digital-fair-value/market-offset-011-5m-30s/events.ndjson
results/short-duration-digital-fair-value/market-offset-011-5m-30s/run-plan.json
results/short-duration-digital-fair-value/market-offset-011-5m-30s/report.json
results/short-duration-digital-fair-value/market-offset-011-5m-30s/calibration-split-report.json
```

旧版 run-plan 不允许悄悄追加；重启同一新版批次仍沿用原始期限，不能通过重启延长直到盈利。

## 运行与验收

先用公开数据验证当前 live 窗口的来源可用性，无须钱包或 API key：

```bash
./target/release/short_duration_digital_fair_value opening-probe btc,eth
```

报告追加为独立新文件 `results/short-duration-digital-fair-value/opening-probes/probe-*.json`。若指定已存在的文件名，命令拒绝覆盖。`verified=true` 只代表开盘来源身份/规则/时间检查通过，不证明交易信号可靠。

然后运行新观察批次：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_next_round.sh observe market-offset-011
```

先运行 2–4 小时验收采集链路；不要把这段时间当作盈利验证期限。检查：

- 同一窗口官方 page opening 与本地 opening TWAP 是否相符；信号前审计是否在原 10s/0.5bps 门禁内。
- 检查点计划、实采、失败与中断能否对账，缺模型样本仍有双次原始盘口。
- source/data timestamp 没有未来数据；不使用 `closePrice` 或事后标签作为信号输入。
- TWAP/spot 健康记录、首次数据、断线原因与每个资产的实际间隙是否一致。
- 单边盘口占比是否依旧很高。若是，则保持 market-offset 候选“不支持”的结论；更早检查点/无需 midpoint 的模型属于另一个预注册实验，不能在本批次临时换模。

停止并同步后生成报告、刷新结算标签及固定模型复核：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_next_round.sh report market-offset-011
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_next_round.sh calibrate market-offset-011
bash scripts/run_digital_fair_value_next_round.sh validate market-offset-011
```

若标签尚未全部闭合，稍后再次 calibrate/validate；不更改原始日志。未满足盈利门禁之前，不进入 canary 或真实交易。

## 部署文件

需要替换以下运行文件（本次未自动上传、启停服务器）：

```text
.env
target/release/short_duration_digital_fair_value
scripts/run_digital_fair_value_next_round.sh
scripts/run_digital_fair_value_research.sh
scripts/digital_fair_value_next_round.py
scripts/digital_fair_value_terminal_study.py
scripts/digital_fair_value_residual_study.py
scripts/digital_fair_value_risk.py
scripts/digital_fair_value_observation_audit.py
```

服务器原有 009 固定模型、protocol、report、parity-report 保持不变；不需要上传 Rust 源码，不需要新增 Python 依赖。

## 本地验证

- 全库 Rust 单元测试 381 项通过；Python SDDFV 测试 50 项通过。
- 新增页面身份/时间/规则/终值拒绝、分片 JSON 解析、缺开盘被动采集、连接静默字段、计划尾部水位、单边盘口与空模型批次报告测试。
- 新代码重放原 010 数据：58 个有效模型样本、78 个在线/离线预测一致；原始数据及固定模型哈希未改变；仍没有通过盈利门禁的候选。
- 本地回归报告：`results/short-duration-digital-fair-value/capture-011-regression-010-2026-10-04/`。它是旧数据的回归检查，不是 011 新服务器运行结果。
- 最终 release 的 Rust/Python 推理校验：3940 个输入、5145 个预测，最大误差约 `2.22e-16`。
- 实时探针报告：`results/short-duration-digital-fair-value/opening-probes/probe-20261004T143049.991Z.json`。UTC 14:30:55 的 BTC/ETH Gamma 和官方页面均 HTTP 200；Gamma 开盘字段缺失，页面的合格 live opening query 也缺失。完整页面分别用时 0.86/2.21 秒，最终仍 `verified=false`，未使用历史终值作开盘证据。
- **官方开盘来源可用性尚未通过验收。** 页面某次存在合格 query 不等于每个窗口持续提供；本地 no-cache/唯一 URL 参数诊断也未取得该窗口的合格 query，因此未把这些手段加入自动采集或修改来源时间戳。新版本只能继续做 observe 采集与来源验收，不能称为已经修复了全部盈利验证障碍。
- 最终 release SHA256：`49702b096e1bd2cc5ea3cdaeef25bf7c7b053e06f39e69c84df83e615d83d334`。
