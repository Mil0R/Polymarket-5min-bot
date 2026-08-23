# 第十阶段优化与完善结果

## 1. 阶段目标

本阶段补齐极小额真实资金 canary 前的代码侧安全门槛：专用硬限额、累计风险熔断、不确定写请求全局暂停、严格 readiness，以及顺序执行状态决策测试。

本阶段没有连接真实账户或提交真实订单。

## 2. Canary 硬限额

新增 `LiveSafetyGuard`，默认由 `CANARY_MODE=false` 关闭。启用后在任何账户写入前原子检查并保守消耗额度：

- 单订单对计划名义金额，默认 3 USDC，代码绝对上限 10 USDC；
- 单进程累计计划名义金额，默认 3 USDC，代码绝对上限 20 USDC；
- 保守损失预算，默认 2 USDC，代码绝对上限 5 USDC；
- 每窗口最多一个订单对；
- 单进程默认最多一个、配置最多三个订单对。

计数在失败后不会回滚，避免失败重试穿透 canary 总量。真实 canary 强制使用 FOK 或 FAK；GTC/GTD 配置会在启动校验阶段被拒绝。

保守损失预算包括负 edge 的配对成交损失和未配对成交腿的全部买入成本。它不是会计已实现 PnL，而是 canary 的最大下行预算。

## 3. 不确定写请求全局暂停

`OrderPairResult` 新增 `write_uncertain`，第一腿传输错误使用可识别的 `UncertainWriteError`。以下情况会触发进程级不可逆安全闸门：

- `post_order` 请求结果无法确认；
- GTC/GTD 余量撤销失败；
- 累计成交查询失败，无法可靠确定第二腿数量；
- 第二腿提交或余量状态无法确认。

`post_order` 结果不确定时，执行器还会对当前订单对的两枚 token 执行尽力撤单；无论补偿是否成功，安全闸门仍保持熔断，不能据此自动继续交易。

闸门触发后：

1. 所有后续窗口的机会准入均视为暂停；
2. `/readyz` 返回 503；
3. 不在进程内提供解除接口；
4. 操作员必须重启并经过现有 startup journal、CLOB 累计成交和 Data API 持仓对账。

## 4. Readiness

可观测性端点现在区分：

- `/livez`：服务进程能够响应即返回 200；
- `/readyz`：所有实盘前置条件满足才返回 200；
- `/healthz`：兼容旧调用，语义等同 `/readyz`；
- `/metrics`：保留 Prometheus 指标。

readiness 同时要求：组件初始化、行情与用户订单流已连接、journal 可同步写入、journal 所在磁盘至少保留 64 MiB、没有未解决订单对、全局实盘安全闸门未触发。journal、磁盘、未解决订单和安全闸门状态在主循环中持续刷新。

## 5. 顺序执行状态机测试

第一腿后的决策被抽为显式状态：

```text
未冻结或无法核对 → PauseUncertain
确认零成交       → StopUnfilled
成交低于最小名义 → RecoverDust
确认且可冻结成交 → SubmitOpposite(size)
```

测试覆盖困难腿排序、确认成交量向下取整、禁止过度对冲、窗口/进程 canary 次数、名义金额限制、单腿下行预算熔断、不确定写入熔断，以及 readiness 各健康条件。

## 6. 当前边界

1. 代码已经具备开始极小额 canary 的硬防线，但仍必须先完成只读 soak 和 `DRY_RUN` soak。
2. 写请求没有交易所级幂等键；当前采用遇到不确定状态立即停机核对的保守替代方案。
3. 困难度仍基于最佳卖盘深度，不是完整成交概率模型。
4. 首次 canary 必须使用隔离账户、人工监控，且不得提高默认额度。

## 7. 验证结果

- `cargo fmt --all -- --check`：通过；
- `cargo check --all-targets`：通过；
- `cargo clippy --all-targets -- -D warnings`：通过；
- `cargo test --all-targets`：66 个测试通过（库 7、主程序 59）；
- `bash -n scripts/dry_run_soak.sh`：通过；
- `git diff --check`：通过。
