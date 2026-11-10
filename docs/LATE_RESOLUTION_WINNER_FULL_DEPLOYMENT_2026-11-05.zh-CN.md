# 2026年11月5日：Operator与紧凑TUI完整部署

按用户“上一轮也没重启，完成一次完整替换和重启”的指示，已在aws-irl服务器完成二进制、启动脚本和独立Operator报告脚本替换，并正常重启策略和TUI。当前批次仍为server-low-latency-008，没有清空、迁移或回填旧研究账本。

## 文件与进程

服务器根目录：`/home/ubuntu/poly`。替换文件：

- `target/release/late_resolution_winner_trading`
- `scripts/run_late_resolution_winner_price_cap.sh`
- `scripts/late_resolution_winner_operator_report.py`

新二进制SHA256：`267800a51f3432fb602fb92eeec06f799909b0ad6e6434a3ea7185cc47b0bb7f`，32,714,480字节。

旧磁盘二进制为60d0f147…，旧策略进程仍使用此前2cb4465c…内存映像；本次重启后策略与TUI都使用新二进制。旧文件均已备份。

- 旧策略PID：216284，以SIGINT正常退出，等待在途任务收尾，没有强制kill。
- 新策略PID：227827，tmux `0:0.0`；启动确认时间（北京时间）：2026-11-05 15:15:19。
- 新TUI PID：227909，tmux `0:1.0`；界面默认为英文候选轨道总览，保留L切语言。
- 新run id：`bb919d4c-fac2-49b9-b9e5-c76e579f7e29`。
- 停止至新后端就绪约40.4秒，属于本次授权重启的采样中断，不追认为已采集数据。

## 数据与Operator

停止时原两组均没有pending成交或执行未知。停止前后及新进程启动后，原`price-cap-study.ndjson`既有前缀SHA256一致；新run只继续追加记录。`.env`SHA256保持`1bb23d78707a4e58bbcb57ea4e1c8bd178cb3372a33441609bae40cb37b7cdea`，没有修改。

原冻结两组继续按5 USDC规则运行。新建`operator-paper.ndjson`独立模拟账本，初始100 USDC现金，单笔含费预算5 USDC，默认暂停，参数revision=0。验证时只打开并取消确认弹窗，没有发送恢复或额度修改命令。

**O切换到Operator时盈亏、交易数为0，是新轨道从零统计，不是清空旧账本。** 从Operator按R一次查看原对照，按R两次查看原候选；P只控制Operator新入场，原冻结研究仍继续。完整操作见[Operator控制说明](LATE_RESOLUTION_WINNER_OPERATOR_CONTROLS_2026-11-05.zh-CN.md)。

## 界面改动和验证

- 面板保留左右一格内边距，取消额外上下空白行；盈亏、行情、健康面板高度同步收紧，头部根据实际行数调整高度。
- 交易分页按可见高度计算，80×24也可在总览显示精简交易表，不隐藏已读取的交易行。
- 15项TUI回归测试通过；release构建通过；现场检查英文及中文、紧凑面板顶部、Operator恢复确认框及取消操作均正常。
- 新后端实时socket已连通，循环推进、市场WS连接；独立检查BTC/ETH TWAP均有有效价格，参考年龄约2.1秒，重启后未见system ERROR。首次检查尚未进入最后60秒，扫描计数0属于正常门控行为，不等于停止采集。

## 回执

服务器备份与回执：`/home/ubuntu/poly/deployments/winner-controls-spacing-20261105-37909387`。

本地回执、上传文件校验值、部署脚本、中英文现场终端快照及操作弹窗：`results-incoming/tui-spacing-deployment-20261105/`。原始账本只保留在原路径，未被本地旧数据覆盖。
