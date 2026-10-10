# SDDFV 009：BTC / ETH 分阶段优化与验收

日期：2026-10-04。仅适用于 BTC / ETH、5 分钟市场、最后 30 秒检查点。

后续采集修订见 [010 开盘证据与观察模式审计](SHORT_DURATION_DIGITAL_FAIR_VALUE_CAPTURE_010.zh-CN.md)。新运行默认使用 market-offset-010，仍沿用本文的 009 开发模型；不要将新版采集追加到旧 009 批次。

## 当前结论与阶段状态

已完成本地代码、回放、报告和阶段门禁；**没有完成新的服务器前瞻验证，也没有验证出可实盘盈利的模型**。

| 执行计划 | 已完成 | 尚待验证 |
|---|---|---|
| 1. 资金和报告 | 有限本金、跨资产共同预算、持仓占用、确认后释放、重启恢复、单写者锁；逐候选实际指标和失败原因 | 真实订单及链上赎回不在本阶段 |
| 2. TWAP 时间结构 | 收到时间与源时间双重 as-of 过滤；120 秒缓存、每来源每秒一桶；信号和到期后片段分别记录 | 007/008 没有完整每秒桶，不能重建全部退出成分；官方聚合权重尚未确认 |
| 3. 本地模型 | 固定终值经验基准与两个 market-offset ridge 候选；按时间分段、手续费与成本压力回放、一致性检查 | 本地盈利门禁未通过，禁止冻结为已验证策略 |
| 4. 新批次 | `observe`、受门禁控制的 `freeze/start`、独立目录和固定截止时间 | 没有自动部署或启动服务器；`observe` 是第 2 阶段补证据，不是盈利验证通过 |
| 5. 验收 | 每资产 100 笔、至少 5 个完整 UTC 日、费用/开盘价缺失排除、按日块区间、收益集中度与压力 PnL | 需要未参与开发的新数据；不能用历史留出微利代替 |

007、008 数据已被多次查看，本轮全部作为**开发数据**。60/20/20 时间划分用于排查，不得称为未见过的独立盈利证据。同一 5 分钟窗口不拆入不同分段。

## 本地结果

共 3,940 个 BTC/ETH 有效终值 episode。所有成交、费用、PnL 都是模拟。

| 固定情景 | 选择段 BTC+ETH 净 PnL | 历史留出段 BTC+ETH 净 PnL |
|---|---:|---:|
| 终值经验基准 | -$98.56 | +$50.92 |
| Market offset ridge 10 | -$18.46 | -$30.00 |
| Market offset ridge 100 | $0.00，零成交 | $0.00，零成交 |

较强收缩的零成交不是盈利。基准留出微利也不能覆盖选择段亏损，且没有击败市场概率。BTC/ETH 的 selected 均为空，`freeze` 按设计返回失败，不生成 `freeze.json`。

完整模型和分段指标：

```text
results/short-duration-digital-fair-value/market-offset-009-development-v2/
  model.json                  固定开发候选，不代表通过盈利门禁
  report.json / report.md      同样本概率评分、逐候选失败原因、成本压力回放
  trades.ndjson                逐候选模拟成交，不可跨候选相加
  next-round-report.json       阶段状态、共同资金账本与参考片段审计
  analysis.zh-CN.md            中文结果摘要
  ledger.ndjson                各独立情景的资金占用/模拟释放事件
  protocol.json               新批次执行假设与验收协议
  inference-fixture.ndjson     无结算标签的推断输入
  parity-report.json           Rust/Python 一致性证据
```

## 资金与成本口径

| 项目 | 固定研究设置 |
|---|---:|
| 虚拟初始本金 | $1000 |
| 单笔含费及滑点支出 | 最多 $5 |
| BTC+ETH 总未结算持仓成本 | 最多 $20 |
| 每资产未结算持仓成本 | 最多 $10 |
| 离线现金释放 | 假设市场到期后 120 秒，**不是实测链上赎回** |
| 延迟盘口 | 实测信号后至少 1 秒、至多 2 秒的 linked REST 盘口 |
| 基准成本 | 10 bps 滑点、75% 深度折扣、2% 模拟失败 |
| 压力成本 | 20 bps 滑点、50% 深度折扣、4% 模拟失败 |

实际只读运行不提交订单、不占用真实资金。旧 Gaussian 在线模拟分支也受虚拟账本控制；重启恢复未结算占用并继续查证。未观察到官方结算不能自动释放现金。

预算可配置，但不能中途修改冻结实验：新建开发工件时可指定 `--initial-balance`、`--max-exposure`、`--max-asset-exposure`。运行脚本使用工件锁定的值，而不是静默接受后来修改的 `.env` 来改变同一批次的资金假设。

未结算库存按买入成本列账，报告的 `nav_cost_basis` / `max_realized_drawdown` **不是可成交盯市净值或完整库存风险**。组合回放 BTC/ETH 共用一笔本金；不同模型是互斥情景，不能合并收益。压力回放会改变成交集合，不是严格收益下界；没有真实 2 秒盘口时，不伪造 2 秒延迟结果。

历史费用缺少当时的逐市场参数，只能标为静态假设。新前瞻主要 PnL 要求信号前已知的 Gamma 费用参数和官方开盘参考价；缺失、过期、身份不符或开盘偏差超过 0.5 bps 都排除，不能默认零费。

## 模型和 TWAP 审计

`market_offset_logistic` 使用有效市场中间价作为基准，以终值预测相对市场的偏差、标准化行权价距离、参考价年龄拟合有界修正。修正幅度最多 0.10，ridge 固定为 10 / 100；只在训练段拟合，在选择段选择，历史留出不重新拟合或改阈值。

缺失中间价或条件特征时不预测、不补零，也不将互补 ask 区间伪装成中间价。单边盘口仍保留用于审计和终值基准对照。

新增记录：

- `signal.reference_buckets`：当时已知、最多 120 秒的每秒来源桶。
- `signal.market_fee_audit`：按 slug / condition ID 校验的费用参数与来源时间。
- `signal.opening_reference_audit`：信号前官方 price-to-beat 与参考流开盘价的偏差。
- `terminal_reference_window`：到期后约 10 秒保存的片段，只用于后验聚合/终值审计，**不能进入预测特征**。

每秒 Chainlink spot 平均值只是待检验的重建代理，不能替代官方 TWAP。误差小也不会自动把 `aggregation_rule_verified` 改为 true。下一步先收集新桶、比较信号与到期片段，再决定是否有依据构建已知/未知成分模型。

## 本地与服务器命令

已生成开发目录，重复 `study` 不允许覆盖。要重新训练，使用新目录并明确纳入的输入：

```bash
python3 scripts/digital_fair_value_next_round.py study \
  --input results/short-duration-digital-fair-value/market-residual-execution-007-5m-30s/events.ndjson \
          results/short-duration-digital-fair-value/terminal-calibration-008-5m-30s/events.ndjson \
  --output results/short-duration-digital-fair-value/market-offset-009-development-NEW
```

当前可执行的一致性检查与缺失证据采集：

```bash
bash scripts/run_digital_fair_value_next_round.sh parity

# 当前候选未通过门槛：只用 observe 补齐第 2 阶段数据。
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_next_round.sh observe market-offset-010
```

如果服务器二进制放在项目根目录，将 `SDDFV_BINARY` 改为 `./short_duration_digital_fair_value`。服务器需 Python 3 标准库和 GNU `timeout`；无需 Rust 源码或编译环境。v2 模型不能直接用二进制 `dry-run` 绕过协议。

首次运行创建 `run-plan.json`，截止时间固定为首次开始后 14 天，重启不延长。可以 Ctrl+C 提前停止；同一模型、模式及协议可继续，改变参数或模式必须用新批次。建议先跑 2–4 小时检查费用、官方开盘价和信号/到期桶是否齐全，而不是先宣称已开始 100 笔盈利冻结验证。

未来只有本地门槛通过后，才可执行：

```bash
bash scripts/run_digital_fair_value_next_round.sh freeze
SDDFV_BINARY=./target/release/short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_next_round.sh start NEW-VALIDATION-BATCH
```

`start` 仍是只读前瞻验证，不是真实下单。每资产至少 100 笔有效独立市场成交、5 个完整 UTC 日、正的基准及压力净 PnL、同样本概率优势、日块区间支持正收益，才允许研究升级。14 天到期仍不足是“证据不足”，不能延长到盈利才停止；频繁查看报告也不会重置协议。

同步后分析：

```bash
bash scripts/run_digital_fair_value_next_round.sh report market-offset-010
bash scripts/run_digital_fair_value_next_round.sh calibrate market-offset-010
bash scripts/run_digital_fair_value_next_round.sh validate market-offset-010
```

验证输出带时间后缀，不覆盖已有结果。全新前瞻样本必须晚于开发数据结束、run ID 不得复用，在线预测需与冻结 Python/Rust 概率一致。当前未选出候选的观察批次即使某个反事实场景微利，也不能在这批数据上重选候选并声称验证通过。

## 路径与服务器所需文件

`.env` 的 SDDFV 默认切到 BTC/ETH 与 009，其他策略配置未修改：

```text
logs/short-duration-digital-fair-value/market-offset-010-5m-30s/system.log
results/short-duration-digital-fair-value/market-offset-010-5m-30s/events.ndjson
results/short-duration-digital-fair-value/market-offset-010-5m-30s/report.json
results/short-duration-digital-fair-value/market-offset-010-5m-30s/calibration-split-report.json
```

新模型/协议必须用新目录；旧 008 的模型、日志与结果未改写。仍保留至少 1 GiB 剩余磁盘空间保护和 2 GiB 单 journal 上限。

运行 `observe` 需要复制：

1. `.env` 和 `target/release/short_duration_digital_fair_value`。
2. `scripts/run_digital_fair_value_next_round.sh`、`scripts/run_digital_fair_value_research.sh`。
3. `scripts/digital_fair_value_next_round.py`、`scripts/digital_fair_value_terminal_study.py`、`scripts/digital_fair_value_residual_study.py`、`scripts/digital_fair_value_risk.py`，以及新增的 `scripts/digital_fair_value_observation_audit.py`。
4. 开发目录中的 `model.json`、`protocol.json`、`parity-report.json`；若要在服务器核对 freeze 失败原因，也复制 `report.json`。

这些 Python 文件是运行时工具，不是服务器编译所需源码。仅复制 `.env` 和二进制不足。本次未自动同步服务器、启动进程或下单。

## 本地校验记录

- 完整 Rust 库测试 371 项通过；Python SDDFV 测试 39 项通过。
- release 可执行文件已编译：`target/release/short_duration_digital_fair_value`。
- v2 一致性：3,940 个案例、5,145 次预测，最大概率差约 `2.22e-16`。
- v1 回归：3,622 个案例、18,010 次预测，一致性通过；旧 008 的 18 个资产/候选组合概率及执行指标与原报告逐项完全一致。
- 当前 `freeze/start` 被正确拦截；直接运行 v2 `dry-run` 也会要求阶段协议。没有生成盈利通过标记或运行服务器前瞻验证。
