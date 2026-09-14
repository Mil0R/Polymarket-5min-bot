# Short Duration Digital Fair Value v5：新鲜度修复与 007 批次

## 本轮范围

实施 10/30/60 秒对照分析发现的数据有效性修复。没有拟合新的概率模型，也没有修改波动率下限、5 USDC 单笔预算、费用、滑点、执行等待或失败模拟参数。60 秒模型校准仍是后续研究，不宣称已完成。

## 代码修改

1. REST fallback 和延迟模拟执行共用请求函数。6 秒总超时覆盖连接、响应头、正文下载和 JSON 解码，REST 耗时也覆盖完整过程。
2. 两条路径都强制校验返回 token 与预期 token、返回 market 与发现的 conditionId。缺失字段不再默认为匹配。
3. 必须提供有效、正整数毫秒时间戳；未来时间戳和超过 `MAX_BOOK_AGE_MS` 的盘口拒绝。当前 `.env` 仍为 1000ms，不将此参数放宽来制造成交。
4. UP/DOWN 并发取回后重新检查两腿时间，避免先返回的盘口在等待另一腿时过期；模拟成交落盘前再次检查盘口年龄、到期时间和参考 feed 年龄。
5. 保存请求开始/接收时刻、完整请求耗时、token/market、hash 与 Date/Age/Cache-Control/CF-Cache-Status/X-Cache 响应头（只保存实际返回值）。常规 fallback 的证据仅保留买卖各前 5 档及总档位数；执行审计保留完整档位。
6. 新增 `execution_book_audit` 关联 evaluation_id，使报价/深度拒绝也能回看执行响应。常规旧盘口拒绝记录到文件和窗口汇总，不逐条打印 WARN。
7. 启动和成交记录的 `execution_version` 升为 5。

时间戳新鲜是必要的数据准入条件，不证明实盘订单必定成交。WebSocket 当前本地接收年龄检查仍保留，最终执行统一通过新的 REST 校验。交易所/本地时钟偏差可能导致保守拒绝，需要根据审计证据调查，不默认容许未来时间。

## 报告口径

`overall` 保留全部原始模拟结算，保持旧报告可审计性；不能仅凭它决定实盘准入。

新增：

- `eligible`、`eligible_by_asset`：通过本版证据要求的样本。
- `by_execution_exchange_age`：≤1 秒、1–2 秒、>2 秒、未知/未来盘口分组。
- `by_reference_feed_age`：相同的参考 feed 年龄分组。
- `data_quality.by_quality_asset_source`：按有效性、资产、WS/REST 来源汇总。
- `data_quality.exclusion_reasons_overlapping`：排除原因，可重叠，不应相加当作独立成交数。

当前报告的有效样本要求：执行版本恰为 v5；记录通过 REST 身份验证且身份字段实际一致；执行盘口年龄 0–1000ms；参考 feed 年龄 0–2000ms；完整 REST 耗时不超过 2000ms。该报告研究门槛固定、明确写入输出，不会因运行时放宽配置而被静默放宽。

运行时参考 feed 上限仍为 5000ms，允许收集研究样本；2–5 秒参考数据产生的模拟成交会保留在 overall，但不进入 eligible。v4 缺少新版本信号/执行校验保证，即使事后计算某笔时间戳新，也只作为历史研究数据，不自动晋级。

`canary_gate` 只使用 eligible 的收益、评分与分资产样本数，并继续要求账目完整性、各启用资产至少 50 笔等原有条件。少量盈利不能绕过这些要求。这里的 eligible 仍不是实际成交能力证明。

## .env 与脚本

只更新该策略的四个日志/结果路径，切换为 `server-low-latency-007-5m-10s`；默认仍为 10 秒。其他策略路径和额度不变，旧 006 日志与结果不删除。

`scripts/run_digital_fair_value_research.sh` 默认批次改为 007。脚本拒绝默认启动 60 秒 dry-run，但允许读取其旧报告；确需专项研究时可显式设置 `SDDFV_ALLOW_60S_RESEARCH=1`。这只是启动脚本护栏，不是移除了二进制的 60 秒能力，也不会停止已在服务器运行的旧进程。

## 部署和双轨运行

先在服务器原终端正常停止旧版本进程；本次没有登录服务器或停止任何远程进程。

复制以下三个文件到服务器项目目录：

- `target/release/short_duration_digital_fair_value` → `./short_duration_digital_fair_value`
- `.env`
- `scripts/run_digital_fair_value_research.sh` → 同名脚本路径

在项目根目录两个独立终端运行（每组只能有一个进程，不要再额外启动默认 10 秒实例）：

```bash
bash scripts/run_digital_fair_value_research.sh 10
```

```bash
bash scripts/run_digital_fair_value_research.sh 30
```

日志目录分别为：

```text
logs/short-duration-digital-fair-value/server-low-latency-007-5m-10s/system.log
logs/short-duration-digital-fair-value/server-low-latency-007-5m-30s/system.log
```

结果目录分别为：

```text
results/short-duration-digital-fair-value/server-low-latency-007-5m-10s/
results/short-duration-digital-fair-value/server-low-latency-007-5m-30s/
```

启动/普通重启追加事件，不需删除文件夹。报告按命令生成并覆盖同组 report.json；不要把 v5 数据写回 006。

本地下载后分别生成报告：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value bash scripts/run_digital_fair_value_research.sh 10 server-low-latency-007 report
SDDFV_BINARY=./target/release/short_duration_digital_fair_value bash scripts/run_digital_fair_value_research.sh 30 server-low-latency-007 report
```

将最后一个参数换为 `calibrate` 会增量补齐官方结算缓存，再生成校准报告。首次补标签可能需要较长时间。

## 验证与下一轮目标

21 项相关测试通过，覆盖身份/时间戳缺失与不匹配、1000/1001ms 边界、未来时间戳、旧版本报告隔离、完整正文耗时、正文卡住超时，以及旧盘口无法进入信号与模拟成交两条路径。

release 二进制已编译。新版 report 对 006 三组做了离线回归，原始成交数与 PnL 保持不变，历史 v4 的 eligible 均为 0；时间戳分组与本次分析一致。回归报告另存于 `results/short-duration-digital-fair-value/comparison-006-20260913/report-v5-{10,30,60}s.json`，没有覆盖原生旧报告。合成冒烟样本还验证了“原始 PnL +90，但有效 PnL -5”时 canary 仍关闭；合成数据只保存在临时目录，不属于策略收益证据。

部署后先确认启动记录 `execution_version=5`，检查旧盘口拒绝是否进入汇总、eligible 与 overall 是否被正确分组。拒绝增加或短期零成交可能是把此前不能可靠定价的样本正确排除，不应立即放宽阈值。

先运行 24 小时检查数据质量，再视有效样本量收集 48–72 小时共同窗口。下一轮重点是有效报价覆盖、模型与市场的同样本比较和排除异常后的收益集中度；不单纯追求成交数。校验修复不保证策略盈利，60 秒模型仍需独立校准。
