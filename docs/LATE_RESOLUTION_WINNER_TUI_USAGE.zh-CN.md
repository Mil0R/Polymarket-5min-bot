# Late Resolution Winner：低开销TUI与独立模拟控制

Operator PAPER 轨道独立记账，支持暂停和恢复新入场、修改下一窗口的模拟额度及追加模拟本金。操作需要确认并由后台写入账本后生效。原冻结轨道继续只读。

已实现独立查看器，读取007/008等批次的price-cap v1 PAPER研究账本。候选与对照隔离，不将主Shadow、严格Paper或其他策略的统计混入。当前不提供实盘操作，不启动交易认证，不创建额外行情订阅，不存储历史行情曲线。

## 服务器替换与启动

当前控制功能升级需要三份文件，并重启后台一次：

1. `target/release/late_resolution_winner_trading`
2. `scripts/run_late_resolution_winner_price_cap.sh`
3. `scripts/late_resolution_winner_operator_report.py`

`.env`无需修改。原Python报告脚本、历史journal保留，不覆盖或删除研究数据。用户手动停止旧进程、覆盖文件后，在原tmux窗口启动策略：

```bash
chmod +x target/release/late_resolution_winner_trading
bash scripts/run_late_resolution_winner_price_cap.sh check server-low-latency-008
bash scripts/run_late_resolution_winner_price_cap.sh dry-run server-low-latency-008
```

在另一个tmux窗格、项目目录启动查看器：

```bash
bash scripts/run_late_resolution_winner_price_cap.sh tui server-low-latency-008
```

查看指定日期、对照轨道：

```bash
bash scripts/run_late_resolution_winner_price_cap.sh tui server-low-latency-007 --date 2026-11-02 --arm control
```

默认批次`server-low-latency-007`。非默认批次在策略和查看器两边都传同一批次名。查看器Q/Ctrl-C只退出查看器，不向策略发送停止命令。停止策略仍由用户手动执行。

**旧进程必须替换二进制并重启后才提供实时快照。** 单独启动新版查看器也可以读取旧账本，但会明确显示离线，不推断策略运行健康。

## 页面与对齐

- **总览**：分隔线分为头部、健康、盈亏、当前窗口、交易表；默认盈亏优先。BTC/ETH/合计各一行，金额右对齐，列宽稳定。
- **交易**：按日分页，显示方向、限价、含费均价、投入、结果及净盈亏；宽屏额外显示信号ask。Enter打开原始精度详情，含费用、时间、模型参数和请求耗时。
- **状态**：循环、扫描、结算检查的独立年龄，当前查询目标窗口，待结算/执行未知、最老在途年龄、run信息与本run REST/WS指标。未采集的p99明确标为未采集。

最低80×24，110×34及更宽终端显示更多交易行。各面板左右保留一格内边距，上下不额外插入空白行；不足32行时合并头部与健康信息，总览同时显示盈亏、行情和精简交易表，完整交易表通过`2`进入。太小时给出尺寸提示，不强行挤压表格。支持UTF-8终端和常见中文等宽字体。

顶部 UTC 时钟每秒刷新。交易详情、最新窗口、运行起止和累计区间均以 UTC 显示，盈亏与交易表标题也标明 UTC。底部快捷键帮助统一占一行，随宽度缩短文案，退出键显示`Q Exit`或`Q退出`。状态页使用分组的“项目 / 数值”表格，长诊断按终端字符宽度折行，↑↓或PgUp/PgDn滚动查看。

## 界面语言

默认语言为英文，与系统语言无关。按`L`即时切换英文/中文，在总览、交易、状态、详情和日期输入窗口均可使用。切换保留日期、轨道、资产、当前选择与滚动位置；仅影响当前查看器，不改变策略配置或账本。

也可在启动或导出快照时指定语言：

```bash
bash scripts/run_late_resolution_winner_price_cap.sh tui server-low-latency-008 --lang zh
# English (default): --lang en
```

本轮界面升级只需原子替换`target/release/late_resolution_winner_trading`并重开查看器；兼容008已有的快照服务，后台策略无需为界面改动重启。

## 快捷键

|键|行为|
|---|---|
|Tab / Shift-Tab，或1/2/3|切总览/交易/状态|
|[ / ]|前一天/后一天|
|D|输入 UTC 日期 YYYY-MM-DD；Backspace修改，Enter确认，Esc取消|
|T|回到 UTC 今天|
|A|交易表按全部/BTC/ETH筛选|
|R|切换查看冻结候选/对照/Operator；仅切换视图|
|O|选择Operator独立模拟轨道|
|P / E / F|仅在Operator视图：暂停/恢复、编辑额度、模拟入金；Enter确认、Esc取消|
|↑↓或J/K|选择交易；详情/状态页为上下滚动|
|PgUp/PgDn|交易翻页；详情/状态页滚动|
|Enter|打开/关闭交易详情|
|Esc|关闭详情，或返回总览|
|L|切换英文/中文；默认英文|
|Q / Ctrl-C|仅退出当前查看器|

没有旧多策略TUI的空格启停、暂停全部和Y恢复操作。新版P仅作用于明确选择的Operator独立模拟轨道，不影响冻结研究。

## 数字变化效果

正常按1秒节奏更新；数字显示值发生变化时短暂高亮约450ms，随后恢复，最多增加一次清除高亮的绘制。没有持续闪烁、滚动数字或曲线动画。初次打开、切日期/轨道不会把整屏当作价格变化闪烁。

可关闭高亮：

```bash
bash scripts/run_late_resolution_winner_price_cap.sh tui server-low-latency-007 --no-highlight
```

## 盈亏口径

金额采用Decimal，主屏四舍五入到4位USDC，详情保留原始精度。成本中的交易费已包含，不再次扣除。

当日统计按 UTC 入场日归属已经结算的净收益；迟到结算更新原入场日。`--date` 和日期快捷键均以 UTC 为准。查看当天时，UTC 午夜会自动切到下一天；浏览历史日期时则停在所选日期。选定日期只影响当日收益及交易表，累计始终显示策略起点至最新已读记录，行情与健康始终反映当前连接。切日期/轨道期间先显示加载状态，避免瞬间混用旧轨道的金额。

未成交不计胜场；pending和执行未知不计入已结算利润，并单列占用成本。重启从同一账本恢复，统计不归零。现金=100初始资金+已结算利润−待结算成本−未知执行预留；现金低于100不自动代表累计亏损。收益未扣服务器费用。

当前只支持独立price-cap v1研究journal。错误文件、重复执行/结算、无效资金记录会明确报错并停止累计，不能显示成0收益。写入尚未完成的最后一行暂不统计。交易详情显示确认规则及入场参数；完整逐点确认过程仍保留在原始observed审计记录。

## 低开销及故障隔离

策略与查看器为独立进程。交易进程只增加本地Unix socket和轻量状态标记；没有查看器时不序列化行情快照、不为面板读盘口。打开后约1Hz获取已有内存行情，不额外请求REST；快照缓存、慢客户端超时和连接数量有界。socket仅当前用户可访问，查看器会验证账本批次一致性。

查看器自身在后台线程增量读取journal，初次建立轻量按日摘要与偏移索引，之后按新增事件更新。详细记录仅对当前页读取，详细页缓存有界；轻量偏移索引随交易数增长，不每帧扫描全量历史。关闭查看器停止它的读取与绘制，策略原有持久化与安全监控继续运行。

终端输出在本地数据快照锁外执行，慢SSH不会持有交易进程的状态锁。查看器不创建Tokio多线程交易运行时。

健康区分进程/快照存活、行情新鲜、循环推进和结算推进；扫描年龄在非尾盘时可能较大，不能据此单独判停。循环长时间不推进、目标窗口过期、存在pending但结算检查不推进，均显示异常。实时连接中断时保留账本并明确标离线，旧快照不能冒充新数据。

原固定旧窗口无限重试的调度逻辑本次未改变；新界面可发现和提示该问题，不能把新增监控称为根因修复。CPU/内存/执行耗时的服务器实测仍需部署后比较，不能把刷新降频等同于总CPU减少80%。

## 离线核对与布局导出

不连接策略、不登录API，输出当前账本的终端布局：

```bash
./target/release/late_resolution_winner_trading tui \
  --journal results/late-resolution-winner-trading/server-low-latency-007-5m/price-cap-study.ndjson \
  --date 2026-11-03 --arm candidate --lang zh --snapshot --width 110 --height 34
```

此模式明确显示“只读账本快照；无实时连接”。源journal只读，不修复、不补写、不追加结算。需要查看命令选项时运行：

```bash
./target/release/late_resolution_winner_trading tui --help
```

## 本地验证记录

首版（界面升级前）发布文件SHA256：`cbd1168c928da7589e637386eb22706be988dff4d759d3e981b424d73d4dd417`。首版Rust库测试400通过、1项原有忽略；真实007数据的两个轨道共18组按日及累计金额与独立分析报告核对一致。另以本地模拟IPC服务完成伪终端操作测试，验证切换、详情、故障提示、退出不停止后台及journal字节不变。

本地测试查看器为2线程、约10MB RSS，仅针对当前样本与本机，不作为服务器性能保证。测试未访问账户或市场API，也未对服务器进行部署操作。


## 2026-11-04界面升级验证

- `cargo test --lib winner_dashboard`：13项通过，包含默认英文、启动语言参数、所有视图中切换语言、单行帮助、四边留白、分页容量、双语状态表格及长诊断滚动、金额和账本只读性。
- 实际伪终端操作通过：默认英文，切换总览/交易/状态，详情及日期弹窗内切换语言，110×34缩放到80×24，Q正常退出。输入使用008不可变同步快照，退出后journal的SHA256不变。
- 中英文总览、状态和80×24紧凑布局预览保存在`results-incoming/tui-layout-20261104/`。预览使用旧同步快照，行情区明确离线，不代表服务器当前运行状态。
- 本地验证阶段未连接服务器。随后按用户“直接替换操作”指示完成服务器部署，仅重开查看器；详见`docs/LATE_RESOLUTION_WINNER_TUI_DEPLOYMENT_2026-11-04.zh-CN.md`。研究规则和交易参数未改。

本轮发布构建：`cargo build --release --bin late_resolution_winner_trading`通过。二进制SHA256：`60d0f147fa7cd2d4b7d1d21949b6a8d59b1d71244d50ba7081e4b0c7029cfc95`。
