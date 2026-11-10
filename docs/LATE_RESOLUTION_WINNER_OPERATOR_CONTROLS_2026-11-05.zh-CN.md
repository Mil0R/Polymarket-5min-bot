# TUI暂停、恢复与动态额度：独立Operator PAPER轨道

**后续更新：本功能及紧凑界面已于11月5日完整部署到aws-irl，策略与TUI均已重启，见[完整部署记录](LATE_RESOLUTION_WINNER_FULL_DEPLOYMENT_2026-11-05.zh-CN.md)。下文“本地实现未部署”描述的是最初实现阶段。**

**新增：Operator现支持F模拟入金，使用与资金语义见[模拟入金说明](LATE_RESOLUTION_WINNER_OPERATOR_FUNDING_2026-11-05.zh-CN.md)。**

2026年11月5日本地实现。没有连接服务器、停止服务器进程、部署或修改`.env`，也没有发送真实订单。

## 操作范围

冻结的008两条研究轨道继续保持每笔含费上限5 USDC。新增一条独立的Operator PAPER轨道：BTC报价范围0.96～0.98，ETH报价范围0.96～0.97；独立100 USDC模拟现金，初始每笔含费预算5 USDC，**首次创建默认暂停**。

Operator写入同批次目录的`operator-paper.ndjson`，不改写原`price-cap-study.ndjson`，不计入原冻结对照收益，也不使用严格生产订单账本。它继承现有候选的行情质量、两次TWAP确认、模拟延迟、FOK和深度折扣规则；仍属于忽略模型反转边界/净EV门槛的研究模拟，不是严格生产入场或真实成交保证。

TUI默认仍查看冻结候选。控制必须明确切到Operator，原对照/候选保持只读。这里的暂停是“暂停Operator新增模拟持仓”，原冻结研究和主Shadow继续观察与模拟；关闭TUI不会改变后台状态。

## 按键和生效规则

|按键|行为|
|---|---|
|`O`|选择Operator独立账本|
|`R`|连接新版引擎时，依次切换冻结对照、冻结候选、Operator|
|`P`|弹出暂停/恢复Operator新入场确认；`Enter`提交，`Esc`取消|
|`F`|追加Operator虚拟现金，Enter确认、Esc取消；不改变交易盈亏或运行状态|
|`E`|输入新额度，`Enter`确认，`Esc`取消；允许1.00～100.00 USDC、最多两位小数|
|`Esc`|返回总览并清除回执提示|
|`Q` / Ctrl-C|只关闭TUI，不暂停或结束后台|

后台确认写入账本成功后，TUI才显示`ACK`。显示栏包含实际生效的暂停状态、额度、版本、待生效额度及在途数量。旧引擎、断线、过期快照或参数版本冲突时不能静默操作；需刷新后重新确认。超时提示结果未知，先检查最新状态，不能把没收到回执解释为命令一定没有执行。

- **暂停/恢复**：立即更新新入场门禁。采集、模型评估、观察记录、已有持仓结算继续。恢复清除旧确认，需要新的两个不同TWAP样本。
- **在途竞争**：写入新的模拟成交前，后台以同一状态锁复核暂停状态和入场epoch。暂停后即便立即恢复，旧epoch未提交的模拟结果也记录为零敞口`error`并释放预留金额，不能再变成新持仓；已经写入的成交继续正常结算。
- **额度**：从后台接收命令时计算的下一个五分钟窗口起点生效；待生效期间旧额度仍有效。额度激活会记录新版本并清除旧确认。已创建的尝试保留原预算，已有持仓不重算；单个市场仍只允许一次尝试，取消后不自动重试。
- **现金和深度**：额度是含费成本上限，实际份额受报价、最优档深度和现金限制。100 USDC是本版本Operator输入硬上限，不是收益外推或加仓建议。没有足够现金时不入场。
- **重启**：暂停状态、生效额度、待生效额度、版本及现金从独立账本恢复；只有第一次创建默认暂停。旧进程的命令不能操作新run。账本损坏、配置冲突或持久化失败时拒绝新入场/操作，不重置余额冒充新实验。

当前控制仅作用于PAPER轨道，不提供实盘启用、真实订单取消、平仓、赎回、解除实盘风险熔断或调整原冻结报价/模型门槛的接口。

## 服务器需要同步的文件

用户手动停止旧策略和TUI，再覆盖：

1. `target/release/late_resolution_winner_trading`
2. `scripts/run_late_resolution_winner_price_cap.sh`
3. `scripts/late_resolution_winner_operator_report.py`

已有`scripts/late_resolution_winner_price_cap_report.py`保留，它也是新离线报告的标准库辅助依赖。无需同步Rust源码或Cargo文件，不覆盖历史结果。`.env`无需修改。

策略主进程必须使用新二进制**重启一次**，才会创建独立轨道及控制接口。只替换磁盘文件或只重启TUI，不能给旧进程增加控制能力。此后TUI参数操作无需重启策略。

原tmux策略窗格：

```bash
cd /home/ubuntu/poly
chmod +x target/release/late_resolution_winner_trading
bash scripts/run_late_resolution_winner_price_cap.sh check server-low-latency-008
bash scripts/run_late_resolution_winner_price_cap.sh dry-run server-low-latency-008
```

另一个tmux窗格：

```bash
cd /home/ubuntu/poly
bash scripts/run_late_resolution_winner_price_cap.sh tui server-low-latency-008
```

进入后按`O`，核对Operator默认状态；首次创建显示暂停，按`P`再按`Enter`开始独立模拟。`E`输入`10`再按`Enter`登记下一窗口的10 USDC预算。历史日期只改变账本视图，控制弹窗作用于**当前后台进程**。

也可直接打开Operator视图：

```bash
bash scripts/run_late_resolution_winner_price_cap.sh tui server-low-latency-008 --arm operator
```

## 版本与收益审计

每个Operator尝试记录`parameter_revision`、`budget_limit`和`entry_epoch`；交易详情显示参数版本与含费预算上限。盈亏面板累计包括该Operator账本的所有版本，不应当成当前额度的单独收益。分析按版本分组使用：

```bash
bash scripts/run_late_resolution_winner_price_cap.sh operator-report server-low-latency-008
```

报告逐事件校验参数变化、命令版本、资金预留、成交与结算，输出现金、未知执行、错误、pending、分资产和分参数版本结果。暂停取消的零敞口错误不会算成负场。原冻结研究报告仍用原`report`命令，不能把两类账本混算或修改旧报告的固定5 USDC口径。

## 开销与接口

现有快照socket保持只读。新增独立本机Unix控制socket，同用户校验，私有0700目录及0600 socket权限；请求有长度上限、读写超时、并发上限，重复ID幂等，过期revision/错误run/错误账本拒绝。控制记录在阻塞工作线程执行持久化，不在TUI绘制线程执行文件写入。

不新增行情订阅、Web服务或历史图。相较旧版增加一个本机命令监听socket（原先已有快照socket）；Operator自身需要额外的观察落盘，以及运行时的独立模拟读取/结算工作，因此不是零开销。服务器扫描连续性仍需部署后的真实日志验证，不能用本地单测承诺负载不变。

## 本地验证与文件校验

- `cargo check --all-targets --offline`通过。
- Rust默认425项通过，另两项跨语言账本审计显式运行通过。
- 原冻结报告与新Operator报告合计10项Python测试通过。
- release构建通过；实际终端PTY验证恢复、额度输入、下一窗口待生效状态、暂停、过小终端禁止确认、Q退出后后台接口继续存活。
- 新二进制离线读取真实008账本：候选累计+4.1276、对照+0.4772，与前次独立Decimal审计一致。15个输入journal及`.env`SHA256保持不变。
- 这些是本地模拟接口及账本验证，未连接或部署服务器，未宣称已完成服务器负载测量。

首次本地构建二进制：32,676,112字节，Linux x86-64；SHA256：

```text
ed9ac119dbf7baa70402edca62bd6672b684a030591d5ff41a633de79a504951
```

完整文件清单、源文件指纹及验证结果：`results-incoming/winner-operator-controls-20261105/release-manifest.json`；同目录保留构建、测试日志、终端回执及真实账本的80×24/110×34快照。

既有固定目标市场发现重试风险不属于本轮控制功能修复范围。
