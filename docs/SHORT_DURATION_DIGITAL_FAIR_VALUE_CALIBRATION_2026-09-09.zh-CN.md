# 概率校准与对照实验：006 批次

本轮实施优先任务 1、2 的审计基础以及 3 的对照运行工具。尚未拟合/替换到期概率模型、自动选择资产参数或启用新的风控停止阈值。3 笔亏损不能作为选择最优参数的依据；原有滑点、部分成交、随机失败模拟继续保留。

## 新增记录和分析

每个模型就绪市场窗口在盘口准入之前记录首次观察，包括 UP 基准概率、开盘/当前 TWAP、实际剩余秒数、参考来源及新鲜完整 WS 盘口的中间价概率（缺失则为 null）。因此不要求该窗口最终成交；仍不包含模型始终未就绪的窗口，不能称为覆盖所有计划市场的无偏样本。

新增 `calibrate` 命令：

- 按 slug 查询官方终值；验证 closed、Up/Down 标签与终态价格，不能把未结算算成失败。
- 缓存 reference-audit.json；每取得一个可用结果原子保存，重启复用。仅同一目录运行一个刷新进程。
- 按资产/时间组/feed 年龄/概率桶报告 Brier、Log Loss、平均概率与胜率；paired_model 和 paired_market 使用相同可用盘口样本比较。其他分组样本可能不同，不能直接比较其分数。
- 记录官方 finalPrice 相对当前 TWAP 的残差、开盘参考偏差，汇总残差均值和 RMS。
- 按 market/checkpoint 跨重启保留文件中首次观察，标记重复与未关联结算。10/30/60 组可能是同一市场，不能当作独立交易合并。
- 无下限概率仅诊断，不自动更换交易模型；旧 005 数据缺少基准概率和当前 TWAP，新报告明确标记缺失。

## 运行

更新服务器二进制、.env 和 scripts/run_digital_fair_value_research.sh。默认 .env 已指向 006-5m-10s，直接运行 `./short_duration_digital_fair_value dry-run`。

三个独立终端分别执行（每组一个进程，不要再重复启动默认 10 秒组）：

```bash
bash scripts/run_digital_fair_value_research.sh 10
bash scripts/run_digital_fair_value_research.sh 30
bash scripts/run_digital_fair_value_research.sh 60
```

每组系统日志在 logs/short-duration-digital-fair-value/server-low-latency-006-5m-{10,30,60}s/system.log，结果在 results/short-duration-digital-fair-value/对应批次/events.ndjson。普通重启追加。每个时间组是在最后 N 秒内持续扫描，不是固定瞬间立即交易。

下载结果后，在本地为某一组生成收益报告及校准报告：

```bash
SDDFV_BINARY=./target/release/short_duration_digital_fair_value bash scripts/run_digital_fair_value_research.sh 30 server-low-latency-006 report
SDDFV_BINARY=./target/release/short_duration_digital_fair_value bash scripts/run_digital_fair_value_research.sh 30 server-low-latency-006 calibrate
```

第二条会访问 Gamma 更新官方结算缓存；无缓存的旧批次首次可能有数百至数千次顺序请求，控制台显示进度。只读已有缓存可直接用 `short_duration_digital_fair_value calibrate`，默认不会联网。输出 calibration-report.json，与收益 report.json 分开。

先收集至少完整跨日样本，观察模型就绪覆盖、官方标签覆盖、同样本市场基准评分及终值残差，再按训练/验证日期分开进行模型校准。未通过样本外验证不调整实盘准入。新工具不保证 30/60 秒组盈利，也不将三组收益合并为账户收益。

## 本地验证

16 项相关库测试通过，启动脚本语法检查及 diff 检查通过，release 二进制已编译。独立合成观察样本搭配一个真实市场的官方终值进行了命令行冒烟验证：跨重启重复观察去重、官方标签关联、同样本 Brier 计算、终值残差和离线缓存复用均通过。测试产物在 calibration-smoke-20260909，与正式批次隔离，不能用于盈利结论。
