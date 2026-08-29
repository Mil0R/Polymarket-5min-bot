# TUI 策略控制台改造结果

## 目标

TUI 已由“利润大屏 + 多个趋势图”调整为面向运行和验证的策略控制台。界面只展示当前数值，不再绘制 Sparkline、PnL Flow、Exposure Trend 或 Market Edge Curves。

## 布局

界面从上到下分为：

1. 全局状态：DRY RUN/CANARY/LIVE、运行时长、窗口倒计时、UTC，以及 Market WS、User WS、RTDS、Safety 健康状态。
2. 共享市场快照：资产、YES/NO ask、总价、套利 edge、订单簿本地接收年龄。
3. 三策略面板：Binary Complement Arbitrage、Late-Resolution Winner Trading 和 Wallet-Copy Trading 各自展示运行状态、额度、漏斗、拒绝原因、成交和收益。
4. 最近事件：只保留紧凑的运行、成交、结算、重连和人工控制事件。
5. 快捷键提示。

市场快照中的“机会”按 `YES ask + NO ask < 1.000` 判断。只有两边价格都存在、仍在新鲜度窗口内且总和严格小于 1 时，整行才以红色粗体显示；单边缺价或陈旧价格不会触发提示。

策略面板将策略名称固定在左侧、运行状态固定在右侧，指标采用统一的标签宽度和数值右对齐，便于纵向比较。

最近事件的每一行均带 `[HH:MM:SS]` UTC 时间，与顶部 UTC 时钟保持一致。

界面采用三级响应式布局：

- 桌面模式（宽度至少 105 列且高度至少 34 行）：三个策略左右排列并展示指标；
- 中等模式（宽度至少 70 列且高度至少 32 行）：三个策略上下排列，指标采用紧凑格式；
- 手机模式（至少 `40×22`，但不足中等模式）：只显示当前选中策略，市场表压缩为 ASSET/YES/NO/SUM/EDGE，使用 `1/2/3` 切换策略；
- 小于 `40×22`：显示旋转手机或缩小字体的提示，不再静默裁剪数据。

手机 SSH 建议使用横屏；竖屏只要终端达到 `40×22` 也能显示核心信息。

## 策略状态

- `RUNNING`：数据和安全条件健康，允许新准入。
- `PAUSED`：人工暂停，不允许新准入。
- `BLOCKED`：策略开关已打开，但连接、journal、磁盘、未决订单或 safety gate 不满足要求。
- `UNAVAILABLE`：当前进程没有初始化该策略依赖。

顶部 `HEALTH` 标签不再使用固定灰色，而是按服务汇总状态动态显示：全部正常为绿色，存在 `WARN` 为黄色，存在 `ERR` 为红色。各服务圆点仍保留各自的状态颜色。

## 操作

| 按键 | 操作 |
|---|---|
| `↑/↓/←/→`、`j/k` | 选择策略 |
| `1/2/3` | 直接选择 Binary Complement Arbitrage / Late-Resolution Winner Trading / Wallet-Copy Trading |
| `Space` | 启动或暂停选中策略的新准入 |
| `P` | 暂停所有策略的新准入 |
| `Y` | 确认 LIVE/CANARY 策略恢复，确认有效期 5 秒 |
| `Q/Esc` | 退出程序 |

## 安全语义

暂停策略不会停止：

- Market/User/RTDS 数据连接；
- 已提交订单和单腿风险恢复；
- Late-Resolution Winner Trading 已有仓位结算与赎回；
- Wallet-Copy Trading 已有仓位减仓、阶梯退出、盯市和结算；
- journal 持久化和健康检查。

暂停只阻止新订单进入提交流程。Late-Resolution Winner Trading 暂停时还会清除尚未完成的连续确认状态。

策略恢复前检查：

- Market WS 和 User WS 已连接；
- connection supervisor 健康；
- journal 可写且磁盘空间健康；
- 不存在 unresolved order state；
- live safety gate 未触发。

LIVE/CANARY 恢复必须再次按 `Y`，DRY RUN 可直接恢复。

## 启动配置

```dotenv
BINARY_COMPLEMENT_ARBITRAGE_ENABLED=true
LATE_RESOLUTION_WINNER_TRADING_ENABLED=true
WALLET_COPY_TRADING_ENABLED=true
BINARY_COMPLEMENT_ARBITRAGE_BUDGET_USDC=50
LATE_RESOLUTION_WINNER_TRADING_BUDGET_USDC=100
WALLET_COPY_TRADING_BUDGET_USDC=10000
```

配置决定进程启动时的初始策略状态。TUI 修改只影响当前进程，重启后恢复 `.env`。

DRY RUN TUI 会预初始化 Late-Resolution Winner Trading 的只读数据和 paper journal 依赖，因此可以从初始关闭状态热启动。LIVE 模式下，如果 `LATE_RESOLUTION_WINNER_TRADING_ENABLED=false`，Late-Resolution Winner Trading 显示为不可用并要求修改配置后重启，以保证凭据和策略兼容校验不会被绕过。

Wallet-Copy Trading 当前以 PAPER-first 方式嵌入 polypulse：`DRY_RUN=true` 时与另外两个策略一同运行并接受 TUI 控制；`DRY_RUN=false` 时显示 `UNAVAILABLE`，继续阻止三策略共享账户实盘，直到统一账户资金预留和跨策略持仓归属完成。三个策略的 PAPER 现金/风险额度彼此独立，修改额度不会自动修改单笔订单上限。

## 已移除的界面内容

- 大型 Session Profit Hero；
- Session/Window PnL 图；
- Edge、Exposure、Scan Rate、PnL Flow 图；
- Market Edge Curves；
- Exposure Gauge；
- 利润闪烁和套利背景动画；
- Best Trade 等与策略准入诊断无直接关系的主界面数据。
