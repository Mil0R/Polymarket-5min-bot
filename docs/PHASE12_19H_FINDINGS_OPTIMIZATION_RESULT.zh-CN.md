# Phase 12：19 小时运行问题优化结果

> 完成日期：2026-08-23  
> 依据：`STRATEGY_19H_RUN_ANALYSIS_2026-08-23.zh-CN.md`

## 1. 完成内容

### 1.1 Winner Buy 盘口回退

- REST 请求完成后使用新的检查时刻计算 `received_at_ms` 年龄，避免请求前的旧时间把正常响应判为“来自未来”；
- 保持原有 `MAX_BOOK_AGE_MS` 上限，不放宽真正的过期盘口保护；
- 新增指标：
  - `winner_buy_ws_missing_books`；
  - `winner_buy_rest_successes`；
  - `winner_buy_rest_empty_books`；
  - `winner_buy_rest_latency_ms_total`；
  - `winner_buy_rest_latency_ms_max`；
- 增加请求后检查时间、过期和未来时间的回归测试。

### 1.2 Spread 影子候选

- `DepthRejected` 现在携带结构化候选，不再只有一个计数；
- 新增 `ARBITRAGE_SHADOW_LOG_FILE`，当前 paper 路径为 `results/spread-arbitrage/paper/shadow-candidates.ndjson`；
- 每条记录包含 YES/NO 价格、显示 size、两腿 USDC 深度、共同份额、总价、利润比例、盘口年龄/时间差、首次发现时间和连续存在时间；
- 预计算边际 0.1%/0.5%/1%/2%/3% 与深度 1/2/3/5 USDC 网格；
- 同一市场、同一价格组合连续存在至少 250 ms 时标记 `survives_250ms=true`；
- 影子记录只用于研究，不改变当前 3 USDC 深度和 3% 执行边际，也不会触发订单。

### 1.3 结算退避

- 实际 Winner Buy 持仓和反事实评估分别维护按 slug 的查询退避；
- 空结果、未终局和查询失败后依次等待 10、20、40、60 秒，之后保持 60 秒；
- 成功结算后清除对应状态；进程重启仍以 journal 为事实来源。

### 1.4 日志治理

- 文本日志从“仅启动时轮转”改为运行期间持续按大小轮转；
- 配置分策略日志时，系统日志不再重复相同策略事件；未配置分策略文件时，系统日志仍保留这些事件；
- 高频 Spread `Orderbook pair details` 从 debug 降为 trace；
- dry-run `.env` 使用系统 info、Winner Buy 候选/模型 debug；
- 单文件轮转阈值调整为 20 MiB，保留 5 个备份。

## 2. 配置变化

```dotenv
ARBITRAGE_SHADOW_LOG_FILE=results/spread-arbitrage/paper/shadow-candidates.ndjson
RUST_LOG=info,polypulse::strategy::winner_buy=debug,polypulse::winner_model=debug,polymarket_client_sdk::serde_helpers=error
LOG_ROTATE_MAX_BYTES=20971520
LOG_ROTATE_FILES=5
```

交易、风控和 Winner Buy 模型阈值均未放宽。

## 3. 自动验证

- `cargo fmt --all`；
- `cargo test --all-targets`：132 项测试通过（18 个 library + 114 个 binary）；
- `cargo clippy --all-targets -- -D warnings`；
- `cargo build --release`。

## 4. 下一阶段验收

1. 在 VPS 运行 24 小时 dry-run；
2. 检查 REST 成功后不再全部进入 `stale_rest`，并比较 BTC/ETH 无盘口率；
3. 确认 `shadow-candidates.ndjson` 可持续解析，统计阈值网格和 250 ms 存活率；
4. 确认单个文本日志约 20 MiB 时产生 `.1`，且保留数不超过 5；
5. 将新一轮 `logs/` 和 `results/` 拷回分析；
6. 通过 24 小时工程验收后，继续 7～14 天策略有效性采样。

