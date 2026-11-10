# 已优化版本：008 服务器部署记录

2026-11-03 按用户“现在运行已经优化完的版本”指示，已将本轮完成的优化构建为 release，并部署至 `aws-irl:/home/ubuntu/poly`。此前只提出、尚未实现的后续方案没有混入此二进制。

## 需要替换的文件

本次实际只替换一份：

|本地文件|服务器绝对路径|
|---|---|
|`target/release/late_resolution_winner_trading`|`/home/ubuntu/poly/target/release/late_resolution_winner_trading`|

策略与只读 TUI 共用该可执行文件，所以两者都已重启。

下列脚本的服务器哈希与本地相同，无需重复替换：

- `scripts/run_late_resolution_winner_price_cap.sh`：`2a8fd9803502a9a0ed8910a9627d1e128829441f3b26ca0c748906b4b1e0e0c4`
- `scripts/late_resolution_winner_price_cap_report.py`：`beac5a9f59e2e3698107d31dbbe1aa7cac963384f394a667da2dd8e11a263506`

`.env` 没有改写，切换前后文件 SHA256 相同。无需上传 Rust 源码、Cargo 文件、其他策略二进制或本地历史结果。若需要在服务器执行离线校准审计，可额外复制 `scripts/late_resolution_winner_calibration_audit.py`；它不是策略运行依赖。

## 构建、备份与切换

- 构建命令：`cargo build --release --locked --bin late_resolution_winner_trading`，成功。
- 新文件先上传为候选文件，核对服务器 SHA256、`price-cap-info` 和 `--help` 均通过后再切换。
- 新二进制 SHA256：`2cb4465c7d9bcf439f99433003937383d4e639dae1c740c685339d9bcb328dfb`。
- 旧二进制备份：`/home/ubuntu/poly/deployments/winner-008-20261103/late_resolution_winner_trading.previous`。
- 旧二进制 SHA256：`cbd1168c928da7589e637386eb22706be988dff4d759d3e981b424d73d4dd417`。
- 007 停止时有 164 次历史尝试，没有未知执行或未结算成交，文件尾完整。切换前后原研究账本 SHA256 都为 `ef65417fa24375b5118394a3463ed30567c43b365b69d1a47084c1323aae428d`。
- 007 原始账本和日志留在原路径，没有清空、合并或复制为新账本。

## 当前运行

新批次为 `server-low-latency-008`，纯 PAPER，不发送真实订单。主进程 PID `216284`，TUI PID `216299`，run id `0f24f412-9425-42f9-93f9-88f28a7c2322`。

- 启动时间：北京时间 **2026-11-03 22:58:18**。
- 正式研究市场窗口起点：北京时间 **2026-11-03 23:00:00**。
- 主进程在 tmux `0:0.0`，只读 TUI 在 `0:1.0`。
- 结果：`/home/ubuntu/poly/results/late-resolution-winner-trading/server-low-latency-008-5m/`。
- 日志：`/home/ubuntu/poly/logs/server-low-latency-008-5m/`。

服务器当前执行的命令：

```bash
cd /home/ubuntu/poly
bash scripts/run_late_resolution_winner_price_cap.sh dry-run server-low-latency-008
```

另一个窗格的查看命令：

```bash
cd /home/ubuntu/poly
bash scripts/run_late_resolution_winner_price_cap.sh tui server-low-latency-008
```

脚本默认批次依旧是 007，因此操作 008 必须显式传入批次名；没有为切换而修改原脚本默认值。

注册配置保留服务器现有研究参数：BTC/ETH、最后 60 秒、两次不同 TWAP 时间戳确认、含费单笔上限 5 USDC、两组各 100 USDC 模拟现金、FOK、0.75 流动性折扣、最小深度 10 USDC。对照 ETH/BTC 上限 0.98，候选 ETH 上限 0.97、BTC 上限 0.98。服务器现有开盘容差为 65,000ms，也按原配置保留，没有替换成代码默认值。

008 用于新可执行文件下的独立观察，不作为 007 同一冻结实现的续接；没有自动追认旧数据或登记新的 7/14 天研究期限。

## 启动后验证

已确认新主进程和 TUI 存活、实时面板连接、循环推进、盘口及 RTDS 更新。首次窗口结束快照记录 120 次扫描、12 次评估；15:00:43 UTC 的实时快照显示已进入新窗口，市场 WS 连接正常，REST 补读 4/4 成功，BTC/ETH 已有新窗口开盘参考。启动检查未发现 system ERROR。

这些是运行与连接检查，不代表已积累优化后的收益或已通过长期稳定性验证。非尾盘时扫描计数不增长属于当前策略设计。

服务器部署回执：`/home/ubuntu/poly/deployments/winner-008-20261103/receipt.json`。本地副本及构建源码哈希清单位于 `results/late-resolution-winner-trading/optimization-impact-2026-11-03/deployment/`。
