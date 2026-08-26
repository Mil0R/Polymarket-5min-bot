# 跟单候选钱包发现器

`copy_wallet_scout` 是独立、只读的候选钱包发现程序。它不会读取交易密钥、不会连接下单执行器，也不会修改项目 `.env`。

## 能力

- 按 Polymarket 官方一级分类查询排行榜候选池；
- 同时组合 DAY/WEEK/MONTH/ALL 周期和 PNL/VOL 排序，去重后评价；
- 根据近期交易标题、slug 和 event slug 推断二级分类；
- 评价近期活跃度、独立已结算市场数、数据覆盖率、ROI、Profit Factor、胜率、利润集中度和滚动30天一致性；
- 抽样近期市场的实时盘口，评价点差和目标金额的 Top-5 深度；
- 为每个不合格钱包保存机器可读的拒绝原因；
- 生成只包含 `COPY_DRY_RUN=true` 的 PAPER 候选 env 文件。

二级分类是本地、带版本号的规则分类，不是 Polymarket 官方枚举。历史仓位接口不提供真实开仓时间，因此滚动窗口采用可观测的仓位结算时间。公开接口也不能可靠重建每次信号后5/15/30/60秒的真实可成交盘口，所以当前 `live_execution_fit` 只表示近期市场的**当前**点差/深度适配度，不冒充历史延迟收益回测。

## 编译

```bash
cargo build --release --bin copy_wallet_scout
```

生成文件：

```text
target/release/copy_wallet_scout
```

## 推荐用法

先用研究模式缩小候选池：

```bash
./target/release/copy_wallet_scout \
  --profile research \
  --category POLITICS \
  --subcategory elections,geopolitics \
  --period WEEK,MONTH \
  --order PNL,VOL \
  --leaderboard-limit 20 \
  --max-candidates 30 \
  --top 10 \
  --output results/copy-candidates/politics-research
```

天气市场示例：

```bash
./target/release/copy_wallet_scout \
  --profile research \
  --category WEATHER \
  --subcategory temperature,precipitation,storm-hurricane \
  --output results/copy-candidates/weather-research
```

对已知地址进行定向审核，不查询排行榜：

```bash
./target/release/copy_wallet_scout \
  --profile conservative \
  --category '' \
  --wallets 0x地址1,0x地址2 \
  --output results/copy-candidates/manual-review
```

查看所有参数：

```bash
./target/release/copy_wallet_scout --help
```

## 输出文件

| 文件 | 用途 |
|---|---|
| `candidates.csv` | 人工排序、筛选和比较 |
| `candidate-details.ndjson` | 每个候选的完整结构化评价 |
| `rejections.ndjson` | 未通过硬条件的钱包及原因 |
| `report.json` | 本次运行的可复现快照和部分 API 失败 |
| `recommended-paper.env` | 通过全部硬条件的前 N 个钱包，仅供 PAPER 使用 |

使用建议名单前必须人工查看 `report.json` 中的 `partial_failures`。如果存在上游 API 请求失败，不应把“没有数据”误认为钱包没有交易。

## 筛选档位

`conservative` 默认要求最近48小时活跃、30天20笔交易、10个活跃日、80个独立已结算市场、ROI 至少8%、Profit Factor 至少1.25，并限制回撤和利润集中度。

`research` 放宽到最近7天活跃、30天5笔交易、3个活跃日和30个独立已结算市场。研究档输出只能进入 shadow/PAPER 观察，不应直接用于实盘。

不要同时放宽多个硬条件。如果没有钱包通过，应先查看 `rejections.ndjson` 中出现频率最高的拒绝原因，再单独调整一个参数并保留运行快照。
