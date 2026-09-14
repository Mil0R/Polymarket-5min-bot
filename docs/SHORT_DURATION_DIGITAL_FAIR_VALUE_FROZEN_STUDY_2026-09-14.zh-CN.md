# 固定时间分区的概率校准研究

本次增加独立 Python 标准库模块 `scripts/digital_fair_value_study.py`。现有 Rust `validate` 继续用于单批次诊断；跨批次训练和冻结评估使用本模块，不需要重新编译交易二进制。

## 用法

在项目根目录运行：

```bash
python3 scripts/digital_fair_value_study.py \
  --manifest scripts/digital_fair_value_study_006_007.json \
  --output results/short-duration-digital-fair-value/study-006-007-next
```

输出目录必须不存在，防止覆盖之前的研究。输出 `model.json` 保存训练得到的候选模型、验证选择和源数据哈希；`study.json` 保存全部评分、排除原因、日期分层；`report.md` 是简明比较表。不会调用网络，也不会修改 `.env`。

本次已运行的最终输出为 `results/short-duration-digital-fair-value/study-006-007-v2/`。v1 是初次验证输出，保留未覆盖。

## 分区与模型

清单明确指定 UTC 半开区间，同时间窗口的资产和期限始终使用相同分区：

- 训练：2026-09-11 00:00 至 09-12 00:00。
- 验证：09-12 00:00 至 09-13 00:00。
- 历史复核：09-13 00:00 至 09-14 02:05。

数据来自 006/007 的 10s/30s，合计 4,572 个可用观察。部分 006 数据也位于复核区间，按时间而非批次名划分。区间外观察 1,323 个被排除；本次范围内没有缺失标签或特征排除。每个资产/期限独立拟合；30s 训练 243、验证 136，10s 训练 280、验证 283。交易次数与这些概率观察数不同。

模型族固定为当前模型、波动率下限倍率模型、经验终值残差模型。倍率只在训练期从 0.25/0.5/1 三个值中选择；经验模型从训练期 final/current 的对数收益分布拟合，并采用加一平滑限制极端信心。最终选择只看验证期，候选须同时不劣于当前模型的 Brier 和 log loss；选择后不使用复核标签重拟合。无下限概率始终只用于诊断。

保留当前参考源 crypto_prices_twap_sixty 的语义：这是平滑价格过程，逐秒历史增量不等于独立现货收益。本轮经验残差按资产/期限估计到官方 final price 的分布，以检验原先平方根时间波动率假设；不据此断言现有平方根时间模型已被证明正确。参考行情年龄单独分组，残差暂未再按年龄拟合，避免小样本继续细分。

所有报告包含同样本模型/市场评分、Up/Down 概率分桶、参考行情年龄分组、按模型与市场偏差选择方向的分组及逐日 Brier 差值。没有把缺失盘口视为可成交，也不生成替代模型的虚构 PnL。样本仅涉及很少独立日期，因此不能从数量较多的五分钟观察推导可靠长期置信度。

## 本次结果

在历史复核中，配对 Brier 越低越好：

| 资产/期限 | 同报价样本 | 当前模型 | 选定模型 | 市场中点 |
|---|---:|---:|---:|---:|
| BTC 10s | 21 | 0.22567 | 0.22748 | 0.00933 |
| ETH 10s | 8 | 0.24865 | 0.25449 | 0.18900 |
| SOL 10s | 10 | 0.24959 | 0.27767 | 0.03156 |
| BTC 30s | 67 | 0.20485 | 0.16327 | 0.01782 |
| ETH 30s | 54 | 0.22561 | 0.19912 | 0.07557 |
| SOL 30s | 28 | 0.24667 | 0.25231 | 0.11800 |

10s 三个资产选择下限倍率 0.25；30s 三个资产选择经验残差模型。30s BTC/ETH 的概率有所改善，但仍大幅落后市场基准；SOL 配对表现反而变差。整体窗口评分改善没有转化成可报价子集优势。不同期限的市场观察重叠，不能合并为独立样本。

本轮没有通过进入新 dry run 的研究判断。暂不接入 008、不修改交易参数。若未来出现新模型依据，可复用此模块，但不能不断根据已查看的复核结果搜索参数并称为样本外通过。

## 将来的冻结评估

对新数据准备独立清单，所有输入观察时间必须晚于旧模型清单的整个开发/复核结束时间；通过 `--model` 读入冻结模型：

```bash
python3 scripts/digital_fair_value_study.py \
  --manifest path/to/new-data-manifest.json \
  --model results/short-duration-digital-fair-value/study-006-007-v2/model.json \
  --output results/short-duration-digital-fair-value/study-frozen-new
```

此模式只预测、不训练；报告记录父模型哈希。真正的前瞻测试还需要在采样开始前冻结参数及判定规则，仅仅使用晚日期历史数据不能保证研究者未看过数据。

执行收益关卡当前明确为 `NOT_EVALUABLE`，自动实盘提升始终关闭。旧模型的机会审计只覆盖部分潜在信号，缺少新模型完整的延迟成交盘口，因此不能从本轮概率报告得到保守成本后收益结论。

## 008 前瞻冻结验证

008 只运行 30s，固定观察区间为 UTC `[2026-09-14 02:05, 2026-09-21 02:05)`。模型 SHA-256 固定为 `d0de136f4eb66f302e8f2d3d375a88aafc560a16f0fd7d0bd6812fc643d068f1`。启动器会在每次操作前验证模型哈希，使用独立的 `server-low-latency-008-5m-30s` 日志和结果目录。

已于 2026-09-14 03:59 UTC 在 aws-eu 启动，初始 PID 为 `139497`。进程使用 `nohup` 运行；PID 只记录启动实例，不作为结果完整性的依据，最终以事件中的 `run_started`、窗口覆盖和正常停止记录审计。

冻结概率关卡要求每个资产至少 75 个同报价样本、覆盖至少 5 个 UTC 日期。选定模型在每个资产上的 Brier 和 log loss 必须同时优于市场中点与原模型。区间结束前 `finalize` 会拒绝执行；即使概率关卡通过，执行收益关卡仍保持阻塞。

服务器需同时复制 release 二进制、`.env`、`scripts/` 目录以及冻结模型目录 `results/short-duration-digital-fair-value/study-006-007-v2/`。先验证部署：

```bash
SDDFV_BINARY=./short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_frozen_008.sh verify
```

前台启动：

```bash
SDDFV_BINARY=./short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_frozen_008.sh start
```

查看已经同步/采集的数量：

```bash
bash scripts/run_digital_fair_value_frozen_008.sh status
```

观察期结束并停止进程后，生成收益报告、补齐官方结算并运行冻结模型：

```bash
SDDFV_BINARY=./short_duration_digital_fair_value \
  bash scripts/run_digital_fair_value_frozen_008.sh finalize
```

`calibrate` 会访问 Gamma，首次补齐一周市场的官方结果可能耗时较长。最终输出目录固定为 `results/short-duration-digital-fair-value/frozen-eval-008-final/`，已有同名目录时不会覆盖。

验证：六项单元测试通过，覆盖固定时间边界、冻结区间、复核标签不影响模型、经验分布方向与平滑、Up/Down 评分对称性及概率关卡；006/007 全量离线运行成功。
