# Polypulse 第四阶段优化结果

> 目标：执行 P1 工程化重构的前四项  
> 实施日期：2026-08-13

## 1. 阶段结论

本阶段把订单累计成交、仓位成本记账、连接健康和窗口任务生命周期从主循环中的分散逻辑收敛为独立组件。

核心变化：

```text
CLOB HTTP / 用户 WebSocket / 断线补拉
                │
                ▼
       OwnedOrderRegistry
       ├─ 订单用途与 pair 所有权
       ├─ 累计成交单调递增
       └─ 重复、乱序事件去重
                │
                ▼
        PositionTracker ledger
       ├─ 原子买入记账
       ├─ 原子卖出记账
       ├─ 原子双腿 Merge
       └─ RAII 风险预留
```

## 2. 已完成内容

### 2.1 统一 owned-order registry

新增 `risk/owned_orders.rs`，统一保存：

- order ID、pair ID 和 token ID；
- `ArbitrageYes`、`ArbitrageNo`、`RecoverySell` 用途；
- 原始数量、价格和已记账累计成交。

所有累计成交采用单调状态机：

- 新累计值大于已记账值时，只返回差额；
- 相同事件直接忽略；
- 小于已记账值的乱序事件直接忽略；
- 超过订单原始数量的异常累计值被限制在原始数量；
- 重启重复注册同一订单时不会倒退已记账成交。

RiskManager 的套利买单、HedgeMonitor 的恢复卖单、用户 WebSocket 和 HTTP 补拉现在共享同一个 registry。

### 2.2 修复恢复 SELL 事件竞争

统一 registry 后专门处理了以下竞争：

```text
恢复 SELL 用户事件先到
  → RiskManager 观察到累计成交
  → 原子减少仓位和成本
  → 更新恢复快照
  → HedgeMonitor 后续 HTTP 查询识别为重复事件
  → 不再二次扣减
```

反过来，若 HedgeMonitor 的 HTTP 查询先到，后续用户事件同样会被 registry 去重。

恢复卖单完全成交时，用户事件处理器可以直接写入 `recovered`；部分成交则同步持久化 `accounted_sell_fill` 和剩余数量，降低事件后立即崩溃造成的恢复快照落后风险。

### 2.3 原子仓位与成本账本

PositionTracker 新增单一 ledger 锁和三个业务接口：

- `record_buy_fill(token, price, shares)`；
- `record_sell_fill(token, shares)`；
- `record_merge(yes_token, no_token, shares)`。

调用者不再分别调用“改成本”和“改 share”两个公开方法。卖出会根据卖出前仓位按比例扣减成本，并限制最大可卖数量，避免并发或异常累计值制造负仓位。Merge 同时限制在本地两腿共同持仓范围内。

Data API 全量覆盖也在同一 ledger 临界区执行，读取仓位和总敞口不会看到业务操作的中间状态。

### 2.4 RAII 风险预留

原手动 `try_reserve/release_reservation` 改为 `ExposureReservation` guard：

- 成功预留后返回 guard；
- 正常成功、下单失败、策略跳过时自动释放；
- 异步任务被 abort 或 panic unwind 时也通过 Drop 释放；
- 主循环不再维护多个容易遗漏的手工 release 分支。

### 2.5 连接监督模块

新增 `connection_supervisor.rs`，负责：

- 认证订阅健康状态；
- 连接失败与断线状态转换；
- 1–30 秒指数退避；
- 稳定连接后的退避重置；
- 为策略提供统一 `is_healthy()` 开仓条件。

连接安全判断不再与行情过滤混放在 `strategy_gate.rs`。

### 2.6 窗口任务生命周期模块

新增 `window_session.rs` 的 `WindowTaskSet`：

- 集中持有交易 JoinSet；
- 保证每个窗口最多一个 wind-down task；
- 窗口退出时统一等待交易任务；
- 超时后 detach 而不是在双腿之间 abort；
- 最后等待 wind-down 完成。

这是 `main.rs` 拆分的第一阶段。行情选择和 wind-down 业务正文仍在 main，后续还可继续抽为 `WindowSession` 与 `WindDownService`。

## 3. 故障与边界测试

新增确定性故障测试覆盖：

- 部分成交累计值逐步增加；
- 重复用户事件；
- 乱序旧事件；
- 超过订单原始数量的异常事件；
- 重启重复注册不倒退累计值；
- 恢复 SELL 的 registry + ledger 联合去重；
- 超额卖出不会生成负仓位；
- 连接断开立即关闭健康状态并执行有上限退避；
- 窗口交易和 wind-down 任务统一回收；
- 风险预留所在任务被 abort 后自动释放。

这些测试属于进程内 mock/fault model，不连接真实 CLOB。HTTP 协议级 mock server、响应录制和 WebSocket 帧回放仍属于下一阶段。

## 4. 验证结果

```text
cargo fmt --all -- --check       通过
cargo check --all-targets        通过
cargo test --all-targets         26/26 通过
cargo clippy --all-targets       通过，只有既有 too_many_arguments 警告
bash -n scripts/dry_run_soak.sh   通过
git diff --check                 通过
```

没有执行真实订单、撤单、Merge 或链上写交易。

## 5. 后续计划

建议下一阶段继续：

1. 抽出完整 `WindowSession`、`StrategyService` 和 `WindDownService`，继续缩减 main；
2. 为 CLOB、Gamma 和 Data API 建立 HTTP mock server 与固定响应样例；
3. 为用户 WebSocket 建立重复、乱序、断线和帧回放测试；
4. 将 Relayer、钱包和执行器的长参数改为上下文/请求结构体；
5. 加入结构化指标和 NDJSON 日志轮转；
6. 建立 CI，并在长参数重构后启用 Clippy `-D warnings`。
