# PMData 历史数据下载器

`pmdata_downloader` 是独立、只读的 PMData Up/Down 数据下载工具，供
Market Making / Shadow Maker 的 L2 盘口回放、逐笔成交订单流和 markout 分析使用。
它不会读取 Polymarket 私钥，不会下单，也不会在控制台打印 `PMDATA_API_KEY`。

## 运行

```bash
# 下载缺失文件，并刷新仍处于更新窗口内的近期市场，然后退出
./target/release/pmdata_downloader sync

# 每 5 分钟增量更新；Ctrl-C 后再次 sync/watch 会继续
./target/release/pmdata_downloader watch

# 仅读取本地清单，不请求 PMData
./target/release/pmdata_downloader status
```

首次验证建议保持 `.env` 默认的小范围配置：BTC、5 分钟、最近一天，单次最多
20 个市场，每个市场下载 `l2` 与 `trades` 两份文件。确认账号实际 Free Plan
配额和文件大小后，再增加 `PMDATA_MAX_MARKETS_PER_SYNC`、历史天数或资产数。

## 配置

```dotenv
PMDATA_API_KEY=
PMDATA_API_URL=https://api.pmdata.dev
PMDATA_COINS=btc
PMDATA_MARKET_TYPE=5m
PMDATA_DATA_TYPES=l2,trades
PMDATA_HISTORY_DAYS=1
# PMDATA_HISTORY_START_MS=
# PMDATA_HISTORY_END_MS=
PMDATA_DATA_DIR=data/pmdata
PMDATA_DATABASE_FILE=data/pmdata/pmdata.sqlite3
PMDATA_REQUEST_INTERVAL_MS=1100
PMDATA_POLL_SECONDS=300
PMDATA_AVAILABILITY_LAG_SECS=3600
PMDATA_REFRESH_HOURS=2
PMDATA_REFRESH_INTERVAL_SECS=3600
PMDATA_MAX_RETRIES=5
PMDATA_RETRY_INITIAL_MS=1500
PMDATA_MAX_MARKETS_PER_SYNC=20
```

- `PMDATA_COINS`：可选 `btc,eth,sol,xrp,doge,bnb,hype`。
- `PMDATA_MARKET_TYPE`：可选 `5m`、`15m`、`1h`。
- `PMDATA_DATA_TYPES`：`l2` 为增量 L2 盘口；`trades` 为带主动方向的逐笔成交。
- 显式的 `PMDATA_HISTORY_START_MS` 与 `PMDATA_HISTORY_END_MS` 为 Unix 毫秒，适合
  固定、可复现实验，并覆盖 `PMDATA_HISTORY_DAYS`。
- `PMDATA_AVAILABILITY_LAG_SECS` 避免请求尚未发布的最新市场；`PMDATA_REFRESH_HOURS`
  使已结束但仍可能被 PMData 小时更新的窗口重新下载；
  `PMDATA_REFRESH_INTERVAL_SECS` 限制刷新期内的重下频率。刷新期结束后，完整文件会跳过。
- `PMDATA_MAX_MARKETS_PER_SYNC=0` 表示不设上限；Free Plan 应先保持小正数。

## 本地布局与恢复

```text
data/pmdata/
├── downloader.lock
├── pmdata.sqlite3
├── pmdata.sqlite3-wal
└── raw/<coin>/<period>/<slug>/
    ├── l2.parquet
    └── trades.parquet
```

SQLite 的 `markets` 表记录 slug 和窗口；`files` 表记录每个数据类型的状态、文件大小、
SHA-256、路径、下载时间和错误。文件先写入同目录临时文件并 `sync_all`，最后原子重命名；
成功后才更新清单。因此中断、网络失败或 429/5xx 后重跑 `sync` 不会破坏已完成文件。

下载器只生成已结束且超过发布等待期的 crypto Up/Down slug。404/鉴权/配额错误会写入
`last_error` 并使本次 `sync` 失败，但下次可继续重试。429 和 5xx 使用指数退避。

## 回测范围

这些文件适合 queue-aware Shadow Maker 回放：盘口重建、报价触及与前方深度、主动成交量、
成交概率和 5/30/60 秒 markout。它们不包含本策略订单的真实队列位置、撤单优先级或账户
实际 Liquidity Rewards；因此不能将“触价”视为确定成交，也不能单靠历史文件得出奖励后盈利。
