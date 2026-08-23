# 第八阶段优化与完善结果

## 1. 阶段目标

本阶段优先完成四项高价值工作：隔离 CLOB 写路径、提取窗口事件退出状态机、强化状态日志完整性，以及为幂等读取建立统一的有界重试策略。

所有实现与测试均为离线执行，没有连接真实账户或提交任何交易写请求。

## 2. 已完成事项

### 2.1 TradingGateway：隔离 CLOB 写语义

`src/trading/clob_gateway.rs` 新增业务级 `TradingGateway`，向上层只暴露：

- 执行套利订单对；
- 取消指定窗口 token 的活动订单；
- 按指定价格卖出仓位。

官方 SDK 的订单 builder、签名订单类型、签名过程和提交细节仍封装在 `TradingExecutor` 内。`StrategyService` 与 `WindDownService` 现在依赖 `Arc<dyn TradingGateway>`，不再依赖具体执行器类型。

最终报价仍是只读计算流程，由具体 `TradingExecutor` 提供；风险对账继续使用第七阶段的 `OrderStatusReader`。

Fake 测试覆盖：

- 双腿完全成交；
- 单腿成交/另一腿未成交；
- 提交传输错误；
- 窗口订单取消失败。

需要特别说明：下单和取消属于可能改变账户状态的请求，本阶段没有为它们增加自动重试，以避免请求已被服务端接受但客户端未收到响应时重复下单。

### 2.2 WindowEventLoop 退出状态机

新增 `src/window_event_loop.rs`，统一记录窗口循环的第一退出原因：

- 正常跨窗口；
- 用户订单流断开；
- 市场行情流断开。

状态机决定是否进入重连退避，且后续事件不能覆盖首个根因。主循环不再只根据用户流布尔值判断重连；市场流错误或结束也会进入连接退避。

当前已提取退出/重连决策，WebSocket stream 本身及 `tokio::select!` 仍留在 `main.rs`，完整事件循环迁移仍是后续任务。

### 2.3 状态日志版本与校验和

新写入的 journal 记录使用 envelope：

```json
{
  "version": 1,
  "checksum": "SHA-256(event JSON)",
  "event": { "...": "..." }
}
```

改进包括：

- 每条新记录带格式版本；
- 每条 event 带 SHA-256 完整性校验；
- 旧版裸 `JournalEvent` 仍可读取，保持向后兼容；
- 未知版本会被拒绝；
- 中间记录 JSON 损坏或校验失败会阻止启动，避免静默跳过账户历史；
- 仅最后一条记录损坏时，自动将损坏内容保存为同目录 `.corrupt-<timestamp>` 文件，并截断回有效前缀；
- 修复后的 journal 会执行磁盘同步，再重新以追加方式打开。

测试覆盖新版写入、旧版兼容、半行尾记录、校验和错误及中间损坏拒绝。

### 2.4 幂等读取统一重试

新增 `src/network_retry.rs`，提供有界指数退避：

- 默认最多 3 次；
- 初始延迟 200ms；
- 延迟指数增长，最高 2 秒；
- 仅用于幂等读取。

当前已接入：

- Gamma 市场读取；
- Data API 持仓读取；
- CLOB 订单累计成交读取。

可重试条件包括请求超时、连接失败、429、常见 5xx 和临时连接错误。认证错误、校验错误及其他不可识别错误立即返回，不进行反复请求。

测试验证临时 503 在第三次成功，以及 401 只执行一次。

### 2.5 文档同步

中英文 README 已说明 journal 版本/校验机制、损坏尾记录隔离文件及中间损坏的阻断行为。

## 3. 验证结果

- `cargo fmt --all -- --check`：通过；
- `cargo check --all-targets`：通过；
- `cargo clippy --all-targets -- -D warnings`：通过，零警告；
- `cargo test --all-targets`：59 个测试全部通过，其中库测试 7 个、主程序测试 52 个；
- `bash -n scripts/dry_run_soak.sh`：通过；
- `git diff --check`：通过。

## 4. 当前关键边界

```text
StrategyService / WindDownService
              │
              ▼
       TradingGateway
              │
              ▼
       TradingExecutor
              │
        SDK build/sign/post

RiskManager ──► OrderStatusReader ──► SDK order query
                                      └─ bounded read retry

Window select loop ──► WindowEventLoopState ──► exit reason/backoff

StateJournal ──► v1 envelope + SHA-256
             ├─ legacy reader
             ├─ corrupt tail quarantine
             └─ middle corruption fail-closed
```

## 5. 下一阶段建议

1. 将 CLOB 报价 tick 查询也抽为只读 gateway，并将执行器的 build/sign/post 状态机拆成可注入的 SDK adapter，进一步验证真实顺序和取消补偿。
2. 将 WebSocket 建连、订阅和 `tokio::select!` 完整迁入 `WindowEventLoop`，使 `main` 只负责依赖装配和跨窗口调度。
3. 为写请求增加幂等键或提交状态查询机制；在没有幂等保障前继续禁止自动重试下单。
4. journal 增加记录链式哈希、定期快照、压缩归档和隔离文件告警指标。
5. 读取重试增加随机抖动、`Retry-After` 支持和按上游独立的并发/熔断限制。
6. readiness 增加 journal 可写性、磁盘剩余容量、未解决订单数与 CLOB 读取健康状态。
7. 增加 canary 模式硬限额、累计损失熔断和只允许单窗口/单订单对的实盘开关。
8. 在隔离账户执行只读 soak、Dry-run soak 和极小额 canary。

## 6. 结论

第八阶段完成了实盘写语义与业务服务的隔离，并显著增强崩溃恢复数据的可验证性。读取请求现在具备保守、有界的恢复能力，而写请求仍保持“不盲目重试”的安全原则。项目进入真实资金验证前，最重要的后续工作是完整 WebSocket 事件循环迁移、写请求幂等保障和 canary 风控。
