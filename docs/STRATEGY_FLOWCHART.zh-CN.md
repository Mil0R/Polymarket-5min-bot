# Polymarket 5-Minute Bot 策略逻辑流程图与架构设计文档

本文档详细描述了 Polymarket 5 分钟预测市场套利机器人的整体策略逻辑、执行路径与状态机转移流程。

---

## 目录
- [一、系统主事件循环与生命周期](#一系统主事件循环与生命周期)
- [二、核心双腿套利检测与执行流](#二核心双腿套利检测与执行流)
- [三、风控、持仓平衡与止盈止损（Hedge Monitor）](#三风控持仓平衡与止盈止损hedge-monitor)
- [四、尾盘策略：Late-Resolution Winner Trading 与 Wind-Down](#四尾盘策略late-resolution-winner-trading-与-wind-down)
- [五、关键代码模块映射表](#五关键代码模块映射表)

---

## 一、系统主事件循环与生命周期

展示系统自启动、窗口发现、多路事件调度（`tokio::select!`）到窗口轮转的整体架构。

```mermaid
flowchart TD
    Start(["程序启动 main.rs"]) --> Init["初始化组件、鉴权、风控门禁与启动对账"]
    Init --> FetchMarkets["MarketScheduler: 获取当前 5 分钟窗口市场"]
    FetchMarkets --> Subscribe["订阅公共 Orderbook 流 + 用户专属订单 WS 流"]
    Subscribe --> WindowLoop{"进入当前窗口主循环 (tokio::select!)"}

    %% 分支 1: 用户订单流
    WindowLoop -->|"用户订单事件 (WS)"| UserOrderEvent["更新风险账本与未结订单状态 RiskManager"]
    UserOrderEvent --> WindowLoop

    %% 分支 2: 行情深度流
    WindowLoop -->|"Orderbook 更新 (WS)"| CheckHedge["HedgeMonitor: 检查是否有单边持仓触发 TP/SL"]
    CheckHedge --> HandleBook["更新订单簿并触发核心套利评估"]
    HandleBook --> ArbFlow["进入核心套利策略流程 (见流程图二)"]
    ArbFlow --> WindowLoop

    %% 分支 3: 定时持仓平衡
    WindowLoop -->|"定时器 (PositionBalance)"| Balancer["PositionBalancer: 检查单边偏离并调整挂单"]
    Balancer --> WindowLoop

    %% 分支 4: 1秒周期任务
    WindowLoop -->|"每秒轮询 (1s Tick)"| S1Check["更新 UI 倒计时与监控健康度"]
    S1Check --> S1_1{"是否满足尾盘吃单条件? (LateResolutionWinnerTrading)"}
    S1_1 -- "是" --> LateResolutionWinnerTradingExec["Late-Resolution Winner Trading: 买入确定胜方份额 (见流程图四)"]
    S1_1 -- "否" --> S1_2{"是否到达 WindDown 触发点?"}
    LateResolutionWinnerTradingExec --> S1_2
    
    S1_2 -- "是" --> WindDownExec["WindDownService: 撤单 -> 链上 Merge -> 兜底平仓"]
    S1_2 -- "否" --> S1_3{"窗口是否已到期 Rollover?"}
    WindDownExec --> S1_3

    S1_3 -- "是" --> CleanUp["清理旧订阅，结束当前 WindowSession"]
    S1_3 -- "否" --> WindowLoop
    CleanUp --> FetchMarkets
```

---

## 二、核心双腿套利检测与执行流

展示从盘口捕捉（[`ArbitrageDetector`](file:///home/lin/Workspace/Polymarket-5min-bot/src/monitor/arbitrage.rs)）、准入过滤（[`OpportunityAdmissionService`](file:///home/lin/Workspace/Polymarket-5min-bot/src/opportunity_admission.rs)）到分步下单（[`TradingExecutor`](file:///home/lin/Workspace/Polymarket-5min-bot/src/trading/executor.rs)）的完整流转。

```mermaid
flowchart TD
    NewBook["收到 Orderbook 更新"] --> ArbDetect{"ArbitrageDetector: 计算价差与深度"}
    
    ArbDetect -- "YES 卖一 + NO 卖一 >= 1.0 或深度不足" --> RejectArb["放弃本次更新 (NoEdge / DepthRejected)"]
    ArbDetect -- "存在套利空间 (Edge > MinProfit)" --> GateCheck{"准入评估 (AdmissionGate)"}

    GateCheck -- "临近窗口结束 / 价格过低 / 连接断开 / 存在未决状态" --> RejectGate["拦截交易 (AdmissionRejection)"]
    GateCheck -- "准入通过" --> CalcQuote["计算执行报价: 加滑点、Tick向上取整、计算下单量与总成本"]

    CalcQuote --> ProfitableCheck{"加滑点后利润 edge > threshold + fee_buffer?"}
    ProfitableCheck -- "否" --> RejectQuote["报价无利可图，放弃"]
    ProfitableCheck -- "是" --> RiskCheck{"检查总敞口预留 (PositionTracker::reserve)"}

    RiskCheck -- "敞口超限 / 持仓严重失衡 / 交易冷却中" --> RejectRisk["风控拦截"]
    RiskCheck -- "通过" --> ExecFirst["确定难度更高/深度较薄的第一腿 (Harder Leg)"]

    %% 第一腿执行
    ExecFirst --> PreFlight1{"预检: REST 深度是否依然存在?"}
    PreFlight1 -- "盘口已消失" --> AbortPreflight["取消操作，释放敞口预留"]
    PreFlight1 -- "存在" --> PostLeg1["提交第一腿买单 (POST Order)"]

    PostLeg1 --> Reconcile1["冻结并撤销剩余挂单，确认第一腿成交数量 first_filled"]
    Reconcile1 --> CheckLeg1{"第一腿成交情况?"}

    CheckLeg1 -- "完全未成交 (0 股)" --> StopUnfilled["停止，不提交第二腿 (零风险暴露)"]
    CheckLeg1 -- "网络不确定 / 无法确认" --> PauseUncertain["标记 uncertain，触发熔断安全门"]
    CheckLeg1 -- "有效成交 > 0" --> SizingLeg2["以第一腿实际成交量计算第二腿下单量 (hedge_size)"]

    %% 第二腿执行
    SizingLeg2 --> PostLeg2["提交第二腿反向买单 (POST Order)"]
    PostLeg2 --> Reconcile2["冻结并确认第二腿成交数量 second_filled"]
    
    Reconcile2 --> FinalPairResult{"两腿是否完全配对 (YES == NO)?"}
    FinalPairResult -- "完全配对成功" --> ArbSuccess["套利成功! 计入 PnL，等待后续 Merge 释放资金"]
    FinalPairResult -- "部分成交 / 单边成交" --> RegisterRisk["移交风险管理器 (RiskManager 处理敞口)"]
```

---

## 三、风控、持仓平衡与止盈止损（Hedge Monitor）

展示当套利产生不平衡成交时，如何进入风险状态机（[`RiskManager`](file:///home/lin/Workspace/Polymarket-5min-bot/src/risk/manager.rs)）、挂单平衡（[`PositionBalancer`](file:///home/lin/Workspace/Polymarket-5min-bot/src/risk/position_balancer.rs)）以及跟踪止盈止损（[`HedgeMonitor`](file:///home/lin/Workspace/Polymarket-5min-bot/src/risk/hedge_monitor.rs)）。

```mermaid
flowchart TD
    UnbalancedFill["套利产生单边或不平衡成交"] --> JournalLog["持久化到状态日志 (StateJournal)"]
    JournalLog --> CalcImbalance["计算不平衡比例 imbalance_ratio"]

    CalcImbalance --> CheckRatio{"imbalance_ratio > 阈值?"}
    CheckRatio -- "否 (可接受误差)" --> MarkRecovered["标记为 Recovered (接受微小偏差)"]
    CheckRatio -- "是" --> StartHedgeMonitor["启动出场监控 (RecoveryAction::MonitorForExit)"]

    StartHedgeMonitor --> TrackEntry["记录持仓: 标的、持仓量、入场均价、计算 TP价 与 SL价"]
    TrackEntry --> WaitPublicBook["等待该 Token 的最新 Orderbook 买一价 (Best Bid)"]

    WaitPublicBook --> DiffCheck{"反向持仓是否已通过其他途径补齐 (diff <= 0)?"}
    DiffCheck -- "是" --> RecoveredDiff["对冲已完成，移除监控并标记 Recovered"]
    DiffCheck -- "否" --> PriceCompare{"买一价 >= TP价 或 <= SL价?"}

    PriceCompare -- "未触发" --> WaitPublicBook
    PriceCompare -- "触发止盈 / 止损" --> CheckRestingOrder{"是否已有正在处理/挂出的卖单?"}
    
    CheckRestingOrder -- "已有挂单" --> ReconcileResting["对账挂单累计成交，更新剩余待卖量"]
    CheckRestingOrder -- "无挂单" --> PostGTCSell["向 CLOB 提交 GTC 限价卖单 (平仓多余敞口)"]

    PostGTCSell --> CheckSellFill{"卖单是否完全成交?"}
    CheckSellFill -- "全部成交" --> SoldAll["敞口平仓完毕，标记 Recovered 并扣减仓位成本"]
    CheckSellFill -- "部分成交" --> KeepMonitor["保留 order_id，继续在后续循环中对账与跟踪"]

    %% 周期性持仓平衡
    subgraph PeriodicBalance ["后台周期任务: PositionBalancer (默认 60s)"]
        FetchAllOrders["获取当前账户所有活跃买单"] --> CheckMarketPos["获取各市场实际持有仓位"]
        CheckMarketPos --> DiffJudge{"实际持仓差额 >= 阈值 (Case 1)?"}
        DiffJudge -- "是" --> CancelExcess["撤销过剩方的买单"]
        DiffJudge -- "否" --> TotalJudge{"挂单导致总和不平衡 (Case 2)?"}
        TotalJudge -- "是" --> CancelPendingExcess["按价格撤销多余一侧的挂单"]
        TotalJudge -- "否" --> BalanceOK["持仓状态正常"]
    end
```

---

## 四、尾盘策略：Late-Resolution Winner Trading 与 Wind-Down

展示窗口临近到期时，**尾盘胜者吃单**（[`late_resolution_winner_trading.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/late_resolution_winner_trading.rs)）与**尾盘平仓风控**（[`wind_down_service.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/wind_down_service.rs)）的执行路径。

```mermaid
flowchart TD
    WindowTimeCheck["1秒心跳检查剩余时间: seconds_until_end"] --> CheckLateResolutionWinnerTrading{"LATE_RESOLUTION_WINNER_TRADING_ENABLED && seconds_until_end <= 30s?"}

    %% Late-Resolution Winner Trading 分支
    CheckLateResolutionWinnerTrading -- "是" --> FetchRestAsks["REST API 查询 YES 和 NO 的真实买卖深度"]
    FetchRestAsks --> WinnerSignalDetect{"判断胜负信号 (detect_winner_asks)"}
    WinnerSignalDetect -- "胜方最佳卖价在 0.95~0.99 且负方价格 <= 0.1" --> BuildLateResolutionWinnerTrading["生成胜方单边买单 (限价 0.95~0.99)"]
    WinnerSignalDetect -- "胜负未定/两边皆高/无深度" --> SkipLateResolutionWinnerTrading["跳过吃单"]
    BuildLateResolutionWinnerTrading --> PostLateResolutionWinnerTrading["向 CLOB 提交胜方买单"]
    PostLateResolutionWinnerTrading --> HoldToSettle["成交后加入 PendingLateResolutionWinnerTrading 列表，持有至到期结算 ($1.00)"]
    CheckLateResolutionWinnerTrading -- "否" --> CheckWindDown

    %% Wind Down 分支
    SkipLateResolutionWinnerTrading --> CheckWindDown{"WIND_DOWN_BEFORE_WINDOW_END_MINUTES > 0 && 达到设定时间?"}
    HoldToSettle --> CheckWindDown

    CheckWindDown -- "是 (如最后 60s)" --> WindDownStart["启动 WindDown 流程，暂停当前窗口套利开仓"]
    WindDownStart --> CancelAllWinOrders["1. 撤销当前窗口内的所有挂单"]
    CancelAllWinOrders --> FetchPositions["2. 获取当前链上真实持仓"]
    FetchPositions --> OnChainMerge["3. 调用合约/Relayer 对成对的 YES+NO 进行 Merge 解除抵押"]
    OnChainMerge --> DumpRestPositions["4. 遍历剩余单边持仓，按 wind_down_sell_price (0.01) 挂单强制平仓"]
    DumpRestPositions --> EndWindDown["WindDown 结束，等待窗口 Rollover"]
    CheckWindDown -- "否" --> ContinueLoop["继续主事件循环"]
```

---

## 五、关键代码模块映射表

| 模块名称 | 核心源码路径 | 功能职责说明 |
| :--- | :--- | :--- |
| **主启动与事件循环** | [`src/main.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/main.rs) | 负责各子系统组装、WS 多路复用分发及 1s 心跳 |
| **市场调度与窗口会话** | [`src/market/scheduler.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/market/scheduler.rs)<br>[`src/window_session.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/window_session.rs) | 负责 5 分钟市场周期计算、市场发现及窗口生命周期 |
| **套利检测与准入** | [`src/monitor/arbitrage.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/monitor/arbitrage.rs)<br>[`src/opportunity_admission.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/opportunity_admission.rs) | 计算 YES+NO 盘口利润、流动性深度及安全准入检查 |
| **交易报价与两腿执行** | [`src/trading/executor.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/trading/executor.rs)<br>[`src/strategy_service.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/strategy_service.rs) | 报价计算（滑点/Tick）、较薄腿优先执行及对冲两腿下单 |
| **持仓追踪与风险状态机** | [`src/risk/manager.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/risk/manager.rs)<br>[`src/risk/positions.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/risk/positions.rs) | 管理总持仓敞口（Exposure）、订单对状态及日志持久化 |
| **不平衡平衡器** | [`src/risk/position_balancer.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/risk/position_balancer.rs) | 定期比对各市场多空挂单与持仓，撤除不平衡买单 |
| **动态止盈止损监控** | [`src/risk/hedge_monitor.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/risk/hedge_monitor.rs) | 实时监听行情买一价，对单边持仓执行 TP/SL 限价卖出 |
| **尾盘胜方单边吃单** | [`src/late_resolution_winner_trading.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/late_resolution_winner_trading.rs) | 窗口尾声（最后 30s）检测胜负并买入胜方持有至结算 |
| **尾盘风控与清仓** | [`src/wind_down_service.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/wind_down_service.rs)<br>[`src/merge.rs`](file:///home/lin/Workspace/Polymarket-5min-bot/src/merge.rs) | 窗口结束前撤单、链上对冲 Merge 及兜底平仓 |
