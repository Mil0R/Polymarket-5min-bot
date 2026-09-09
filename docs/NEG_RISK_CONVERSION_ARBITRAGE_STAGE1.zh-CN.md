# NegRisk Conversion Arbitrage：第一阶段运行说明

2026-09-09 已增加轮换发现、HTTP/hash 证据和独立诊断组；当前部署路径及新增配置见[诊断优化说明](NEG_RISK_CONVERSION_ARBITRAGE_DIAGNOSTICS_2026-09-09.zh-CN.md)。下文保留第一版参数说明供旧批次对照。

## 范围

这是 Cross-Market Probability Arbitrage 的独立研究扩展，复用 Gamma 发现与 CLOB 盘口客户端。原有 scan/shadow/report 入口和篮子筛选规则保持不变。新程序不读取交易签名密钥、不提交订单、不发起链上转换，且没有 live 命令。

第一阶段回答：在 Gamma 列出的结果集合成立、转换费和等待时间满足假设的条件下，是否有值得进一步核实的转换机会。Gamma 事件成员不等于实际 Adapter 的完整输出集合，所以报告始终标记 `protocol_verified=false`、`live_ready=false`。市场 ID、question/indexSet、部署版本、抵押资产和真实转换费用的链上验证属于下一阶段，不因 Shadow 正收益自动通过。

## 研究路径

单个结果 NO → 其他结果的 YES → 按 bid 卖出。单 NO 转换直接返还现金为零。

- 扫描完整列出且可交易、2–6 个结果的 NegRisk 事件；不继承旧篮子“必须有 Other”的限制。
- 仅识别明确的 Yes/No token；启用费用但缺少费表的事件被排除。
- 使用 5、10、25 份 NO 的数量梯度，单条路径含买入费预算上限默认 25 美元。
- 逐档吃 NO ask，逐档卖其他 YES bid，费用逐档计算。默认可用深度折扣 75%、价格滑点 10bps、假设转换费 0bps、链上成本 0.02 美元。
- 转换费用默认零只是配置假设，不能视为已核实。用非零假设运行敏感性组时须更换 RUN_DIR。
- 候选净收益最低 0.05 美元。只保留每条路径当前收益最高的可执行数量；每条路径 300 秒内最多记录一个候选 episode。
- 候选出现后，在约 2、5、10 秒重新取盘口，用原买入成本计算退出收益，记录实际经过时间。三种延迟是同一候选的备选情景，不能累加成三笔成交。
- 未能完整退出时记录失败原因，并以全部残余输出零回收、损失原始投入加链上成本作为保守情景。该数字不是实际清算价。
- 保存候选与成功延迟退出的盘口证据；记录最接近门槛的未入选路径，便于分析没有机会的原因。

模型假设输入 NO 已经到手、按配置比例完成转换。它没有验证真实链上可用余额、转换成功率、逐腿挂单竞争和库存退出，因此不能将模拟收益解释为锁定的账户利润。

## 编译与服务器命令

```bash
cargo build --release --bin neg_risk_conversion_arbitrage
```

将 `target/release/neg_risk_conversion_arbitrage` 和更新后的 `.env` 复制到服务器同一工作目录。只需公开数据访问；第一阶段不需要真实交易资金。

```bash
# 单次扫描；有候选时也执行延迟退出观察
./neg_risk_conversion_arbitrage scan

# 持续运行；不要求先 scan
./neg_risk_conversion_arbitrage shadow

# 随时读取当前批次并生成报告
./neg_risk_conversion_arbitrage report
```

Ctrl+C 会停止并生成报告。也可以设置自动停止：

```bash
NEG_RISK_CONVERSION_ARBITRAGE_DURATION_SECS=259200 ./neg_risk_conversion_arbitrage shadow
```

持续 72 小时的时间限制会在循环边界检查，进行中的 HTTP 请求或延迟观察可能让退出稍晚。进程被强制终止时，下次启动追加记录，新 run_id 区分运行；未完成的旧 episode 保留为 pending，不冒充完成收益。报告遇到破损 JSON 行会计数并阻止推荐进入下一阶段。

## 保存路径

默认 `.env`：

```dotenv
NEG_RISK_CONVERSION_ARBITRAGE_RUN_DIR=results/neg-risk-conversion-arbitrage/shadow-run-001
```

目录下包含：

| 文件 | 内容与写入方式 |
|---|---|
| runtime.log | 发现、错误、运行汇总；追加，控制台同步显示 |
| events.ndjson | 配置、发现诊断、路径、扫描、盘口、延迟退出；追加 |
| report.json | report 命令或正常结束时生成；覆盖该批次旧报告 |

目录自动创建，不需要删除旧策略数据。运行期间另开终端执行 report 即可更新报告；程序不会每分钟自动重写 report.json。同一 RUN_DIR 避免同时启动两个 shadow 进程。没有自动日志轮转，长期运行应检查磁盘空间并按批次归档。

新批次只需覆盖目录，日志和报告一起切换：

```bash
NEG_RISK_CONVERSION_ARBITRAGE_RUN_DIR=results/neg-risk-conversion-arbitrage/shadow-run-002 \
./neg_risk_conversion_arbitrage shadow
```

分析新批次时，对 report 命令传入相同 RUN_DIR。报告使用当前配置作为收益门槛，并打印配置；不要混合不同参数批次。

## 默认覆盖与运行条件

每 10 秒启动一轮（请求与候选延迟检查会延长实际周期），每 1800 秒重新发现事件。默认扫描 10 页 Gamma 事件、最多 100 条路径、8 个并发请求。发现页数与路径截断诊断均写入 events；采样范围之外没有数据，不能由“零机会”推断全市场不存在机会。

使用 REST 进行可回放的第一版研究，会漏掉短暂机会。低延迟服务器更合适；如大量 `snapshot_skew`，先检查请求延迟及覆盖规模。若扩充事件页数或结果腿数，应另起批次。

每条盘口要求本地接收时间距评估不超过 5 秒，组合本地时间及交易所时间最大偏差默认 2 秒。交易所时间允许领先本机最多 10 秒，单独记录时钟异常和年龄范围；这不是把过期盘口放宽到 10 秒。所有转换研究 REST 响应必须匹配请求 token。

## 阶段门槛

建议先运行 24 小时检查覆盖、费用与时钟诊断，再继续至 72 小时或一周。报告 `size_rejections` 是数量梯度评估次数，不是独立机会数。

程序只在至少 20 个 episode 的全部三种延迟都能完整退出并达到净收益门槛、涉及至少 3 个不同事件、完整 episode 的最坏延迟收益合计为正且没有破损记录时，提示：

`REVIEW_ECONOMICS_THEN_VERIFY_CONTRACT_PATH`

这只是建议人工复核并进入链上只读验证；不自动进入下一阶段。相同事件的反复候选有关联，不应把 episode 数当作独立统计样本。`worst_delay_pnl_sum` 是观察样本的情景合计，不是同一资金账户可同时取得的收益。

证据不足时为 `INSUFFICIENT_EVIDENCE`。应根据发现覆盖、最接近门槛的路径、深度、费用及延迟后失败情况决定扩大观察还是停止，而不是自动降低门槛制造成交。
