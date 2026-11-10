# TUI双语及布局升级：服务器部署记录

北京时间2026-11-04 23:00:35，按用户“直接替换操作”指示完成部署。

- 服务器：`aws-irl:/home/ubuntu/poly`。
- 原子替换文件：`target/release/late_resolution_winner_trading`。
- 新SHA256：`60d0f147fa7cd2d4b7d1d21949b6a8d59b1d71244d50ba7081e4b0c7029cfc95`。
- 旧SHA256：`2cb4465c7d9bcf439f99433003937383d4e639dae1c740c685339d9bcb328dfb`。
- 旧文件备份：`/home/ubuntu/poly/deployments/winner-tui-20261104-203eb0fc/late_resolution_winner_trading.previous`。
- 只读查看器：tmux `0:1.0`，旧PID 216816正常退出，新PID **221991**。
- 当前语言：英文；已现场验证`L`切中文及切回英文，当前停留总览页。
- 策略主进程PID **216284**、启动标识和run id `0f24f412-9425-42f9-93f9-88f28a7c2322`保持一致，未重启。旧进程继续使用其原有内存映像。
- 实时快照已连通，循环推进，市场WS连接正常；本轮没有更改研究准入规则、资金参数、`.env`或启动脚本，也没有清空/重写研究账本。
- 新文件的`price-cap-info`与原文件逐字段一致，`tui --help`支持`--lang en|zh`。

服务器回执：`/home/ubuntu/poly/deployments/winner-tui-20261104-203eb0fc/receipt.json`。

本地回执、部署脚本及中英文终端快照：`results-incoming/tui-deployment-20261104/`。现场使用现有008批次，不新建研究批次。
