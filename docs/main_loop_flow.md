# 主循环业务流程图（完整链路）

## 一、启动阶段（一次性）

```mermaid
flowchart TB
    START(["main() 启动"]) --> TLS["rustls 初始化<br/>+ 日志/TUI 开关"]
    TLS --> CFG["Config::from_env 加载配置"]
    CFG --> INIT["初始化组件<br/>Dashboard / RuntimeMetrics / LiveSafetyGuard<br/>ServiceHealth / MarketDiscoverer / MarketScheduler"]
    INIT --> PK["校验私钥格式 LocalSigner"]
    PK --> AUTH["create_authenticated_clob_client<br/>CLOB V2 认证（EOA 或 Poly1271 代理）"]
    AUTH --> AUTH_FAIL{"认证成功?"}
    AUTH_FAIL -- "否" --> EXIT1["打印排查指引<br/>退出"]
    AUTH_FAIL -- "是" --> BUILD["构建 TradingExecutor<br/>RiskManager / HedgeMonitor / StrategyService"]
    BUILD --> VERIFY["verify_authentication<br/>真实 API 调用验证"]
    VERIFY --> VFAIL{"API 调用正常?"}
    VFAIL -- "否" --> EXIT2["提示账户未注册/受限<br/>退出"]
    VFAIL -- "是" --> RECON["reconcile_startup 启动对账<br/>+ hedge_monitor.restore_from_journal<br/>恢复上次会话的恢复监控"]
    RECON --> BG["启动后台任务"]
    BG --> BG1["position sync 任务<br/>每 N 秒从 API 同步持仓"]
    BG --> BG2["ScheduledMergeService<br/>每 N 分钟 YES+NO 链上合并<br/>（wind-down 进行中则跳过）"]
    BG --> OBS["可选 observability_server<br/>健康检查 HTTP"]
    BG2 --> MAINLOOP
    BG1 --> MAINLOOP
    OBS --> MAINLOOP(["进入窗口主循环 ↓"])
```

## 二、窗口主循环（每个 5 分钟窗口）

```mermaid
flowchart TB
    MAINLOOP(["窗口主循环"]) --> CHK_SHUT{"shutdown?"}
    CHK_SHUT -- "是" --> END(["退出"])
    CHK_SHUT -- "否" --> FETCH["scheduler.get_markets<br/>immediately_or_wait"]
    FETCH --> FERR{"拉取成功?"}
    FERR -- "否" --> SLEEP60["sleep 60s"]
    SLEEP60 --> FETCH
    FERR -- "是" --> EMPTY{"市场为空?"}
    EMPTY -- "是" --> FETCH
    EMPTY -- "否" --> SUB["OrderBookMonitor.subscribe_market<br/>订阅本窗口全部市场 + 恢复监控 token"]
    SUB --> STREAM["create_orderbook_stream"]
    STREAM --> SERR{"建流成功?"}
    SERR -- "否" --> FETCH
    SERR -- "是" --> SESS["WindowSession::new<br/>计算窗口 timestamp / scope / 交易暂停标志"]
    SESS --> USERWS["用户订单流认证订阅<br/>subscribe_orders（auth WebSocket）"]
    USERWS --> UERR{"订阅成功?"}
    UERR -- "否" --> BACKOFF["connection_supervisor.connect_failed<br/>退避后重来"]
    BACKOFF --> FETCH
    UERR -- "是" --> READY["subscription_ready<br/>service_health 置为已连接"]
    READY --> EVTLOOP["内层事件循环 tokio::select! ↓"]
```

### 内层事件循环的四个分支

```mermaid
flowchart LR
    subgraph EVT["内层事件循环 tokio::select! 每窗口一次"]
        direction TB
        B1["分支① 用户订单流<br/>user_order_stream.next()"]
        B2["分支② 订单簿流<br/>book stream（核心套利链路）"]
        B3["分支③ 仓位平衡定时器<br/>每 N 秒"]
        B4["分支④ 每秒 tick<br/>窗口滚动 / wind-down / 健康状态"]
    end
```

**分支① 用户订单流**：

```mermaid
flowchart TB
    E1["收到 OrderMessage"] --> E1A["risk_manager.observe_order_event<br/>更新订单状态 / unresolved 标志"]
    E1A --> E1B["更新 service_health<br/>no_unresolved_orders"]
    E2["流错误 / 断开"] --> E2A["window_session.mark_user_stream_failed<br/>暂停交易（scope.pause_trading）"]
    E2A --> E2B["记录 UserStreamDisconnected<br/>break 退出内层循环 → 走窗口收尾"]
```

**分支② 订单簿流（核心套利链路）**：

```mermaid
flowchart TB
    BOOK["收到 BookUpdate"] --> HEDGE["hedge_monitor.check_and_execute<br/>恢复监控优先消费同一数据流"]
    HEDGE --> PAIR["monitor.handle_book_update<br/>YES/NO 配对 → OrderBookPair"]
    PAIR --> GATE["process_pair：时间戳守卫<br/>validate_book_timestamps"]
    GATE --> GFAIL{"过期 / 未来 / 偏斜?"}
    GFAIL -- "是" --> DROP1["丢弃（debug 日志）"]
    GFAIL -- "否" --> DETECT["ArbitrageDetector.check_arbitrage<br/>YES best ask + NO best ask<br/>edge = 1 − (yes+no)"]
    DETECT --> DFAIL{"edge ≥ min_profit<br/>且单腿价值 ≥ $1?"}
    DFAIL -- "否" --> DROP2["丢弃（无套利）"]
    DFAIL -- "是" --> ADMIT["OpportunityAdmissionService.evaluate"]
    ADMIT --> A1{"total ≤ 执行价差阈值?"}
    A1 -- "否" --> REJ1["ExecutionSpread"]
    A1 -- "是" --> A2{"交易已暂停?"}
    A2 -- "是" --> REJ2["TradingPaused"]
    A2 -- "否" --> A3{"连接健康?"}
    A3 -- "否" --> REJ3["ConnectionUnhealthy"]
    A3 -- "是" --> A4{"有未决订单状态?"}
    A4 -- "是" --> REJ4["UnresolvedState"]
    A4 -- "否" --> A5{"YES/NO 价格 ≥ 最低价?"}
    A5 -- "否" --> REJ5["YesPriceTooLow / NoPriceTooLow"]
    A5 -- "是" --> A6{"距收盘 > stop_before_end?"}
    A6 -- "否" --> REJ6["NearMarketEnd"]
    A6 -- "是" --> DEDUP["SignalDeduplicator.admit<br/>冷却期内相同价格信号去重"]
    DEDUP --> DDUP{"重复信号?"}
    DDUP -- "是" --> DROP3["丢弃"]
    DDUP -- "否" --> OPP["产出 MarketTick.opportunity"]
    OPP --> LOGOPP["异步记录 arbitrage_logger"]
    LOGOPP --> QUOTE["executor.execution_quote<br/>取 tick size + 方向滑点(↓用第二档)<br/>加 min_profit / execution_spread / fee_buffer<br/>重新校验盈利性"]
    QUOTE --> QFAIL{"报价有效?"}
    QFAIL -- "否" --> DROP4["跳过（record_quote_rejection）"]
    QFAIL -- "是" --> RESERVE["position_tracker.reserve<br/>敞口预留 reservation"]
    RESERVE --> RFAIL{"未超 max_exposure?"}
    RFAIL -- "否" --> DROP5["跳过（record_risk_rejection）"]
    RFAIL -- "是" --> BAL["position_balancer.should_skip_arbitrage<br/>本地缓存即时检查"]
    BAL --> BFAIL{"仓位严重失衡?"}
    BFAIL -- "是" --> DROP6["跳过（record_balance_rejection）"]
    BFAIL -- "否" --> INTERVAL{"距上次交易 ≥ 3s?"}
    INTERVAL -- "否" --> DROP7["跳过"]
    INTERVAL -- "是" --> SAFETY["live_safety.try_start_pair<br/>Canary 安全闸（限额/对数）"]
    SAFETY --> SF{"放行?"}
    SF -- "否" --> DROP8["跳过（record_risk_rejection）"]
    SF -- "是" --> UI1["更新 Dashboard / 日志"]
    UI1 --> SPAWN["window_session.spawn_trade<br/>strategy_service.execute(...)"]
    SPAWN --> EXECUTE["执行链路 ↓"]
```

**分支③ 仓位平衡**：

```mermaid
flowchart TB
    T1["定时器到点"] --> T2["position_balancer.check_and_balance_positions<br/>检查持仓与订单，撤除超额订单"]
```

**分支④ 每秒 tick**：

```mermaid
flowchart TB
    S1["每秒触发"] --> S2["更新 Dashboard 窗口倒计时 / 敞口<br/>更新 service_health"]
    S2 --> WIND{"≤ N 分钟到窗口结束<br/>且未开始 wind-down?"}
    WIND -- "是" --> WD1["scope.pause_trading<br/>wind_down_in_progress = true"]
    WD1 --> WD2["启动 WindDownService ↓"]
    WIND -- "否" --> ROLL{"已滚动到新 5 分钟窗口?"}
    ROLL -- "是" --> R1["drop(stream) + monitor.clear()<br/>记录 Rollover 退出原因<br/>break 内层循环"]
    ROLL -- "否" --> S1
```

## 三、下单执行链路（StrategyService.execute）

```mermaid
flowchart TB
    EXECUTE(["execute(opportunity, quote, reservation)"]) --> DRY{"dry_run?"}
    DRY -- "是" --> DRYSIM["模拟全成交 OrderPairResult<br/>success = true"]
    DRYSIM --> REG1["live_safety.record_result"]
    DRY -- "否" --> HARDER["harder_leg：先下较薄(深度的)腿<br/>深度相同则下更便宜的腿"]
    HARDER --> SIGN1["build_and_sign_buy<br/>limit order（GTD/GTC/FOK/FAK）"]
    SIGN1 --> POST1["post_order 第一腿"]
    POST1 --> PERR{"传输错误?"}
    PERR -- "是" --> CANCELA["cancel_orders_for_tokens 尽力撤单"]
    CANCELA --> UNC["UncertainWriteError"]
    UNC --> TRIP["live_safety.trip + 健康检查置位"]
    PERR -- "否" --> FREEZE["freeze_and_reconcile_fill<br/>取消未成交余量 + 核对累计成交"]
    FREEZE --> DECIDE{"第一腿状态机"}
    DECIDE -- "未冻结(成交不确定)" --> PAUSE["PauseUncertain<br/>不提交对侧 write_uncertain=true"]
    DECIDE -- "冻结但 0 成交" --> STOP["StopUnfilled<br/>抑制对侧，返回失败"]
    DECIDE -- "冻结但成交 < 最小额" --> DUST["RecoverDust<br/>返回单腿结果，交风控"]
    DECIDE -- "冻结且有成交" --> OPP2["SubmitOpposite<br/>按已成交额(2位小数)计算对侧 size"]
    OPP2 --> SIGN2["build_and_sign_buy 对侧"]
    SIGN2 --> POST2["post_order 对侧"]
    POST2 --> PERR2{"传输错误?"}
    PERR2 -- "是" --> CANCELB["token 范围撤单<br/>返回 first_only_result(write_uncertain)"]
    PERR2 -- "否" --> FREEZE2["freeze_and_reconcile_fill 对侧"]
    FREEZE2 --> RESULT["汇总 OrderPairResult<br/>success = 双腿均足额成交"]
    RESULT --> REG2["live_safety.record_result<br/>更新 safety_ok"]
    REG2 --> DASH2["Dashboard 记录成功/失败<br/>realized_profit = (1−yes−no)×min成交"]
    DASH2 --> REGPAIR["risk_manager.register_order_pair<br/>记录双腿订单 + 成交"]
    REGPAIR --> HANDLE["risk_manager.handle_order_pair"]
    HANDLE --> ACT{"RecoveryAction?"}
    ACT -- "None" --> DONE1["结束"]
    ACT -- "MonitorForExit" --> HMON["hedge_monitor.add_position<br/>启动恢复监控（持续盯盘直至退出）"]
    ACT -- "ManualIntervention" --> WARN1["告警：需要人工介入"]
```

## 四、窗口收尾与 wind-down

```mermaid
flowchart TB
    FINISH(["内层循环 break"]) --> WAIT["window_session.finish(30s)<br/>等待交易任务 / wind-down 任务收尾"]
    WAIT --> UNRES{"存在未决订单状态?"}
    UNRES -- "是" --> REC1["risk_manager.reconcile_owned_orders<br/>+ hedge_monitor.reconcile_owned_orders"]
    UNRES -- "否" --> SKIP1["跳过对账"]
    REC1 --> R2
    SKIP1 --> R2["更新 service_health"]
    R2 --> REASON{"退出原因?"}
    REASON -- "UserStream/MarketStream 断开" --> BACK["connection_supervisor.disconnected<br/>指数退避 1s→30s（稳定期后重置为 1s）<br/>新交易暂停"]
    REASON -- "正常窗口结束 / Rollover" --> NORM["connection_supervisor.normal_session_end"]
    BACK --> MET["输出 runtime_metrics 快照"]
    NORM --> MET
    MET --> MAINLOOP2["回到窗口主循环顶部<br/>刷新市场进入下一窗口"]
```

**WindDownService 内部流程**（窗口结束前触发）：

```mermaid
flowchart TB
    WD["WindDownService.run"] --> WDC["cancel_window_orders<br/>撤掉本窗口全部挂单"]
    WDC --> WD10["sleep 10s"]
    WD10 --> WDM{"配置了 proxy 地址?"}
    WDM -- "是" --> WDG["get_positions → merge_info<br/>对允许条件内的 YES+NO 仓位<br/>链上 merge_max 合并（每笔间隔 30s）"]
    WDG --> WDS["get_positions → 对允许 token 的残余仓位<br/>以 wind_down_sell_price(默认0.01) GTC 卖出"]
    WDM -- "否" --> WDS
    WDS --> WDD["in_progress = false<br/>监控持续到窗口真正结束"]
```

## 关键状态与闸门汇总

| 阶段 | 守卫/闸门 | 作用 |
|---|---|---|
| process_pair 前 | `validate_book_timestamps` | 快照新鲜度 / 未来时间 / YES-NO 偏斜 |
| 检测 | `ArbitrageDetector` | edge ≥ min_profit、单腿 ≥ $1 |
| 准入 | `OpportunityAdmissionService` | 价差阈值、暂停、连接健康、未决状态、最低价、临近收盘（6 类拒绝） |
| 去重 | `SignalDeduplicator` | 冷却期内相同价格信号 |
| 报价 | `calculate_execution_quote` | tick 向上取整、方向滑点、利润+费率缓冲 |
| 敞口 | `PositionTracker.reserve` | 总敞口 ≤ max_exposure |
| 平衡 | `PositionBalancer` | 持仓严重失衡时跳过 |
| 节流 | 3s 最小交易间隔 | 防止并发刷单 |
| 安全 | `LiveSafetyGuard.try_start_pair` | Canary 限额 / 累计亏损 / 对数上限 |
| 执行中 | 第一腿状态机 | 传输错误→不追加对侧；未冻结→暂停；0 成交→停止 |
| 事后 | `handle_order_pair` | 单腿成交→恢复监控；不确定写→trip |
