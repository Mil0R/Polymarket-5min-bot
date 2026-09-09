# NegRisk Conversion Arbitrage 第一阶段诊断优化

针对 shadow-run-001 的零机会结果，本次完善市场发现、盘口证据和诊断对照。仍是公开数据 Shadow，报告中的 protocol_verified 和 live_ready 保持 false。

## 行为与配置

1. **轮换发现范围。** 每次发现读取 10 页，按 3 个分页范围轮换：offset 页数 0、10、20，再回到 0。本地 `.env` 刷新间隔从 1800 秒调整为 600 秒，一个完整覆盖周期约 30 分钟。实际列表会变化，所以不能保证覆盖 3000 个不同事件；报告以实际唯一事件为准。每个范围仍最多输出 100 条路径。不会把前几个范围保留的路由不断累加。
2. **拆分剔除原因。** 报告分别记录不可交易/缺 token、结果过少、结果过多、启用费用但缺费表、结果标签不合法、重复 token。事件可能命中多个条件，细分计数不能相加当作事件总数。没有取消完整输出集合与最大腿数要求。
3. **采样失败路径证据。** 默认每 60 秒轮换记录最多 5 条路径，每条腿保存 token、响应接收时间、交易所年龄、严格新鲜度失败原因、买卖各前 5 档。请求审计包括开始/结束时间、REST 耗时、hash，以及服务端返回的 Date/Age/Cache-Control/CF-Cache-Status/X-Cache 头。没有返回的字段不补造。候选额外保存完整盘口，失败路径的前 5 档不能保证完整重放深度。
4. **独立诊断组。** 默认代码与示例配置关闭，本地 `.env` 开启。诊断组允许交易所时间戳最多落后 600000ms，不要求不同腿交易所时间同步；仍要求本地接收年龄不超过 5 秒、接收时间差不超过 2 秒、REST 请求耗时不超过 1000ms、交易所时间不能异常领先，且时间戳和耗时必须存在。深度、费用、滑点、额度、最小收益要求全部保留。
5. **重复验证。** 诊断组达到收益门槛时生成 diagnostic_candidate，约 2、5、10 秒后重新请求，保留初始 NO 成本计算退出。每次保存 hash、价格、时间和退出结果。报告计数 same_hash/changed_hash/missing_hash_or_response；同 hash 只说明返回状态相同，不证明订单可成交、缓存无误或转换成功。诊断组完全不参与严格组的下一阶段门槛。
6. **可解释报告。** 增加唯一事件/路径、严格可定价样本数、逐事件数量评估原因、各腿时间失败计数、每轮 REST/交易所年龄分布，以及独立诊断组收益、未完成样本和最好报价。严格组最优报价和诊断组最优报价保存原始价格差，便于区别交易价差与费用/滑点/链上成本。该原始差额仍按假设转换输出数量和深度折扣计算。

严格时间检查原值不变。10 分钟上限只是诊断采样界限，不表示这样的盘口可以执行。新模块未使用 WebSocket 对这些状态做交叉验证，也未新增成交活跃度排序；本轮通过分页轮换扩大覆盖，并提供之后选择活跃事件所需的按事件数据。

## `.env` 修改

```dotenv
NEG_RISK_CONVERSION_ARBITRAGE_RUN_DIR=results/neg-risk-conversion-arbitrage/shadow-run-002
NEG_RISK_CONVERSION_ARBITRAGE_REFRESH_SECS=600
NEG_RISK_CONVERSION_ARBITRAGE_DISCOVERY_WINDOWS=3
NEG_RISK_CONVERSION_ARBITRAGE_DIAGNOSTIC_ENABLED=true
NEG_RISK_CONVERSION_ARBITRAGE_DIAGNOSTIC_MAX_EXCHANGE_AGE_MS=600000
NEG_RISK_CONVERSION_ARBITRAGE_DIAGNOSTIC_MAX_REST_MS=1000
NEG_RISK_CONVERSION_ARBITRAGE_EVIDENCE_INTERVAL_SECS=60
NEG_RISK_CONVERSION_ARBITRAGE_EVIDENCE_ROUTES=5
```

单条输入上限仍为 25 USDC，净收益门槛仍为 0.05 USDC。采样增多会增加文件体积，程序没有自动轮转；正常轮次的证据量受间隔、路径数和 5 档限制，出现候选时会额外保存完整证据。

## 编译、部署与分析

```bash
cargo build --release --bin neg_risk_conversion_arbitrage
```

复制新二进制和 `.env` 到服务器工作目录后运行，无需先 scan：

```bash
./neg_risk_conversion_arbitrage shadow
```

日志/事件追加保存到新批次目录中的 runtime.log 和 events.ndjson；正常停止或 report 命令重新生成 report.json。

```bash
./neg_risk_conversion_arbitrage report
```

本地分析时使用 `./target/release/neg_risk_conversion_arbitrage report`。旧批次必须显式指定旧目录，避免沿用新 `.env`：

```bash
NEG_RISK_CONVERSION_ARBITRAGE_RUN_DIR=results/neg-risk-conversion-arbitrage/shadow-run-001 \
./target/release/neg_risk_conversion_arbitrage report
```

旧数据可读取，但不能恢复未曾保存的 hash、HTTP 头或诊断组价格。报告使用当前参数展示门槛；不同参数组应各用目录保存。

建议新批次运行 24–72 小时。先检查唯一事件数是否增加、每个分页范围的剔除原因、严格与诊断可定价数量，再检查正收益路径在 2/5/10 秒后的深度与 hash。诊断正收益是需要调查的线索，不能作为链上转换可行性已通过的结论。

## 本地验证

- 17 项相关测试通过，release 编译成功。覆盖诊断组仍拒绝旧本地响应、过慢 REST、超过诊断年龄上限的盘口，以及诊断正收益不能触发严格组晋级。
- 新报告重读 shadow-run-001，仍为零候选、68,328 次严格可定价评估、9 个事件；旧批次结果没有因诊断规则被重新解释为成交。
- 约 35 秒公开接口测试使用每范围 2 页、最大 10 路径、5 秒刷新，实际 offset 页数依次为 0、2、4、0、2，对应 5、10、9、5、10 条路由。10 轮扫描及 5 轮证据记录成功保存到 `results/neg-risk-conversion-arbitrage/smoke-diagnostics-20260909/`。
- 测试严格组 234 次数量评估均被时间检查拦截；诊断组 225 次完成定价但未达收益门槛，9 次因 REST 请求超过 1000ms 拒绝。hash、请求开始/结束和实际返回的 HTTP 头均已记录。没有真实候选，尚未在公开行情测试中获得延迟退出样本。
