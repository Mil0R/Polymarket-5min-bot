# Polypulse 优化与完善实施结果

> 实施依据：`docs/CODEBASE_ANALYSIS.zh-CN.md` 第 10 节“建议的演进顺序”
> 实施日期：2026-08-13

## 1. 实施结论

本轮优化已按原建议的优先顺序完成资金安全、风险记账、窗口收尾、配置、代码边界和测试改造。核心交易路径现在是：

```text
最优一档行情
  → 原始价差初筛
  → 应用方向滑点
  → 最终限价 + fee buffer 再校验
  → 按每腿美元上限换算 share
  → 原子预留风险额度
  → 双腿顺序提交
  → 撤销未成交余量
  → 按真实成交记仓位与成本
  → 对单腿/不平衡成交启动退出监控
```

此外，本轮在落实测试时确认了一个原分析未完全暴露的关键问题：Polymarket SDK 的 `asks` 是升序、`bids` 是降序，原代码使用数组末尾作为最优价，实际上会取到最差档。行情展示、套利检测和恢复卖出现在全部改为读取首档。

## 2. 对第 10 节建议的逐项落实

### 2.1 最终限价与盈利校验

状态：已完成。

- 新增 `ExecutionQuote`，统一表示真正提交到 CLOB 的 YES/NO 限价、share 数量、总成本和单份 edge。
- 盈利判断从原始订单簿价格后移到滑点应用之后。
- 最终要求为：

  ```text
  1 - yes_limit - no_limit
      >= max(BINARY_COMPLEMENT_ARBITRAGE_MIN_PROFIT_THRESHOLD, BINARY_COMPLEMENT_ARBITRAGE_EXECUTION_SPREAD)
         + BINARY_COMPLEMENT_ARBITRAGE_FEE_BUFFER
  ```

- 新增 `BINARY_COMPLEMENT_ARBITRAGE_FEE_BUFFER`，默认 `0.002`，用于预留手续费和舍入误差。
- 限价被约束到 `[0.01, 0.99]`，避免负滑点或异常配置产生非法价格。
- `BINARY_COMPLEMENT_ARBITRAGE_MIN_PROFIT_THRESHOLD` 已真正参与 detector 和最终报价判断，不再是无效配置。
- `BINARY_COMPLEMENT_ARBITRAGE_MAX_ORDER_SIZE_USDC` 现在按最终限价换算 share，确保每一腿名义金额不超过配置值。

涉及文件：`src/trading/executor.rs`、`src/monitor/arbitrage.rs`、`src/config.rs`。

### 2.2 订单对状态、真实成交记账与恢复

状态：已完成。

- 下单前通过 reservation 预留风险额度，避免并发任务共同穿透最大敞口。
- 正常响应后释放 reservation，仅按 `taking_amount × 最终限价` 记录真实成交成本。
- 发送或报价失败时释放 reservation，不再永久占用虚假敞口。
- 任一腿未完全成交时，立即撤销两腿尚未成交的订单余量，防止之后异步成交继续扩大失衡。
- 若一腿 HTTP 请求失败而另一腿已获得响应，保留已知成交结果、撤销成功腿余量，并把订单对交给风险管理器；不再直接丢失第一腿信息。
- `OrderPairResult.success` 现在只在两腿均完全成交时为真，仪表盘不会再把单腿成交计作成功套利。
- 部分成交和单腿成交会生成 `MonitorForExit`，已有 HedgeMonitor 已正式接入订单簿；达到止盈或止损价时只卖出真实超额仓位。
- 恢复卖单已禁止重复重挂；已有活动退出单时不会再创建第二张相同卖单。
- 修正恢复卖出数量：手续费影响成交所得，不应从可卖 share 数量中再次扣减。

涉及文件：`src/trading/executor.rs`、`src/risk/manager.rs`、`src/risk/recovery.rs`、`src/risk/hedge_monitor.rs`、`src/main.rs`。

### 2.3 全账户敞口模型

状态：已完成保守模型。

- 删除每个 5 分钟窗口开始时清空敞口的行为。
- Data API 同步仓位时同步重建实际持仓敞口。
- 因 Data API 路径没有可靠成本基准，当前采用保守估值：每个未平 outcome share 按 1 美元计入风险。
- 仓位平衡任务读取全账户活动买单后，以 `price × remaining_size` 汇总活动订单名义金额并覆盖到敞口模型。
- 正在提交但 CLOB 尚未反映的订单由 reservation 计入。
- Merge 或恢复卖出成功后按实际减少的 share 比例扣减成本。

因此当前敞口由三部分组成：

```text
Data API 未平仓位保守成本
+ CLOB 活动买单剩余名义金额
+ 进程内正在提交的订单 reservation
```

涉及文件：`src/risk/positions.rs`、`src/risk/position_balancer.rs`、`src/main.rs`。

### 2.4 Wind-down 开仓闸门与范围限制

状态：已完成。

- 新增 `WindowScope`，集中持有当前窗口 condition ID、token ID 和不可逆的 pause 状态。
- Wind-down 触发后立即关闭当前窗口新开仓；后台收尾结束后也不会重新开启。
- 撤单从账户级 `cancel_all_orders()` 改为仅取消当前窗口 token 的活动订单。
- Merge condition 限制在当前窗口集合。
- 收尾卖出限制在当前窗口 token 集合，不再遍历卖出账户全部正持仓。
- 定时 Merge 与 Wind-down 仍通过全局原子标志互斥。

涉及文件：`src/window.rs`、`src/main.rs`、`src/trading/executor.rs`。

### 2.5 主循环边界和异步任务生命周期

状态：已完成第一阶段拆分。

- 新增独立 `WindowScope` 模块，把窗口资产所有权和开仓闸门从千行主循环中抽离，并附单元测试。
- 每窗口套利任务改由 `JoinSet` 集中持有。
- 窗口切换时最多等待 30 秒回收交易任务；超时任务只 detach、不 abort，避免恰好在双腿之间强制取消。
- Wind-down task 使用显式 handle，切换到下一窗口前等待其完成，避免旧窗口收尾与新窗口交易并发修改账户。
- 删除空的 `trading/orders.rs`、`utils/errors.rs` 模块。
- 清理已废弃的风险状态、无调用 helper 和错误注释。

`main.rs` 仍然承担顶层编排，这是刻意保留的应用入口；链上 Merge 任务和 Wind-down 流程暂未进一步抽成服务，以避免本轮资金安全修改同时引入更大范围的机械迁移。

### 2.6 自动化测试

状态：已完成核心纯逻辑覆盖。

新增测试覆盖：

- 非法订单类型和滑点格式；
- `BINARY_COMPLEMENT_ARBITRAGE_MIN_PROFIT_THRESHOLD` 实际生效；
- 最优 ask 档位和机会边界；
- 滑点吞噬利润时拒绝最终报价；
- 每腿美元名义上限到 share 的换算；
- 风险 reservation 的上限、释放和真实成本留存；
- 活动订单敞口采用替换语义而非重复累加；
- WindowScope 的资产范围和不可逆暂停。

验证命令：

```bash
cargo test --all-targets
cargo check --all-targets
git diff --check
```

真实账户下单、Relayer 和 Polygon 交易不会在自动测试中执行。

### 2.7 严格配置校验

状态：已完成。

- 数字解析失败不再静默采用默认值，而是带变量名和原始值返回错误。
- 未知订单类型不再悄悄退回 GTD。
- 滑点只接受一个或两个合法数字。
- 校验比例范围、正数金额、GTD 有效期、价格范围以及 spread + buffer 组合。
- proxy 地址格式错误会立即报错。
- 非 EOA 交易必须有 funder；EOA 若开启 Data API 仓位同步、Merge 或 Wind-down，也必须提供查询地址。
- 更新 `.env.example` 和中英文 README，明确美元名义上限、全账户敞口与 fee buffer 语义。
- 新增可选 `BINARY_COMPLEMENT_ARBITRAGE_LOG_FILE`，机会记录改为标准 NDJSON；未配置时不产生文件。

涉及文件：`src/config.rs`、`.env.example`、`README.md`、`README.zh-CN.md`、`src/utils/arbitrage_logger.rs`。

### 2.8 HedgeMonitor 与 Redeem 的取舍

状态：已明确处理。

- HedgeMonitor 已接入真实订单对恢复路径和公开订单簿，不再是只构造不用的保留代码。
- Redeem 继续保留为库能力，但本轮没有自动接入 5 分钟主循环。

未自动 Redeem 是有意的安全决定：当前主循环没有可靠的 condition resolution 事件、胜出结果确认和幂等任务存储。仅根据时间或持仓调用赎回会把临时 API 状态、链上 finalization 延迟和重复交易风险带进实时交易循环。后续若接入，应作为独立的已结算市场队列，并持久化 condition 级幂等状态。

## 3. 额外修复

### 3.1 订单簿最优价方向

SDK 类型注释明确：

- asks 按价格升序排列，`first()` 是最低卖价；
- bids 按价格降序排列，`first()` 是最高买价。

原实现的 `last()` 已全部改为 `first()`。这项修复影响套利判断、TUI/日志展示以及恢复退出价格，属于本轮最重要的正确性修复之一。

### 3.2 outcome index 兼容

PositionBalancer 和持仓同步展示现在会按 condition 检测索引约定：

- Data API 约定：0=YES、1=NO；
- CTF 约定：1=YES、2=NO。

避免在 1/2 约定下把 index 1 错当成 NO。

### 3.3 可观测性语义

- 仪表盘成功数只记录两腿完全成交。
- PnL 使用最终限价和两腿共同真实成交量计算，不再使用原始盘口和计划数量。
- 可选机会日志采用每行一个 JSON 对象的 NDJSON 格式。

## 4. 当前仍存在的边界

1. Polymarket CLOB 不提供本项目可用的原子双腿提交，因此两个 HTTP 请求之间仍存在不可消除的时间差；当前通过撤销余量、真实成交记账和退出监控降低风险。
2. 仓位成本采用每 share 1 美元的保守估值，适合限制风险，但不等同于会计成本或净损失价值。
3. 恢复卖单使用 GTC；部分即时成交后，剩余订单保持在 CLOB，不会自动追价制造重复订单。
4. 30 秒后仍未结束的旧窗口交易任务会 detach 而非 abort。这是为了避免在双腿之间取消；日志会明确记录该异常。
5. 自动 Redeem 尚未接入，原因见 2.8。

## 5. 配置迁移提示

- 新增 `BINARY_COMPLEMENT_ARBITRAGE_FEE_BUFFER=0.002`。最终所需 edge 是 spread/min-profit 与该 buffer 之和；原本较激进的滑点配置可能因此不再下单。
- `BINARY_COMPLEMENT_ARBITRAGE_MAX_ORDER_SIZE_USDC` 现在真正表示每腿美元名义上限，同一配置下 share 数量可能与旧版本不同。
- 非法数字、未知订单类型、非法地址现在会阻止启动，请先修正 `.env`。
- EOA 模式若没有 `POLYMARKET_PROXY_ADDRESS`，需要将 `POSITION_SYNC_INTERVAL_SECS=0`、`MERGE_INTERVAL_MINUTES=0`、`WIND_DOWN_BEFORE_WINDOW_END_MINUTES=0`；否则需提供 Data API 查询地址。
- 如需机会文件日志，配置 `BINARY_COMPLEMENT_ARBITRAGE_LOG_FILE=arbitrage.ndjson`。

## 6. 结果总结

本轮将项目从“按计划成本触发、失败后弱恢复、窗口级清零风险”的实现，推进为“按最终报价准入、并发预留、真实成交结算、账户级风险同步、窗口范围收尾和单腿退出监控”的实现。最关键的资金安全问题已经在代码路径上建立明确防线，同时保留了真实市场无法原子双腿成交这一客观限制。
