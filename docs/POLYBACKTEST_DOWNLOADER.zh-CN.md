# PolyBacktest 历史数据下载器

`polybacktest_downloader` 是独立只读工具，用于持续保存 BTC/ETH/SOL
Up/Down 历史市场，供 Binary Complement Arbitrage 和 Late-Resolution Winner Trading 离线回放。它不会读取
Polymarket 私钥，也不会提交订单。

## 运行

```bash
# 下载当前 API 权限范围内、所有配置资产的缺失已结算市场，然后退出
./target/release/polybacktest_downloader sync

# 每 5 分钟检查新市场，Ctrl-C 正常停止
./target/release/polybacktest_downloader watch

# 只查看本地统计，不请求 PolyBacktest API
./target/release/polybacktest_downloader status
```

也可以通过 Cargo 运行：

```bash
cargo run --release --bin polybacktest_downloader -- sync
```

Pro 31-day 配置会按 `POLYBACKTEST_COINS` 依次发现并下载 BTC/ETH/SOL 5m
市场。Free Plan 只能访问其套餐允许的最近市场；Pro 才适合做全量 31 天回溯。
下载器遇到已完成市场会跳过，只为待下载市场请求快照，因此可以中断后继续。

如果机器上仍有旧版单币种 `watch` 进程，先在该进程终端按 `Ctrl-C` 正常停止（或
向确认过的 PID 发送 `SIGINT`），再启动新版本；不要让两个下载器同时使用同一个
数据目录。随后先运行 `status` 检查三个资产，再运行 `sync` 补齐历史数据，最后按需
运行 `watch`。已有 BTC 数据会复用，ETH/SOL 会在同一数据库中新增，不需要删除
`data/polybacktest`。

## 配置

```dotenv
POLYBACKTEST_API_KEY=
POLYBACKTEST_API_URL=https://api.polybacktest.com
POLYBACKTEST_COINS=btc,eth,sol
POLYBACKTEST_COIN=btc
POLYBACKTEST_MARKET_TYPE=5m
POLYBACKTEST_PLAN=pro
POLYBACKTEST_HISTORY_DAYS=31
# 可选：指定固定 Unix 毫秒边界；设置后覆盖 HISTORY_DAYS
# POLYBACKTEST_HISTORY_START_MS=
# POLYBACKTEST_HISTORY_END_MS=
POLYBACKTEST_DATA_DIR=data/polybacktest
POLYBACKTEST_DATABASE_FILE=data/polybacktest/polybacktest.sqlite3
POLYBACKTEST_PAGE_LIMIT=1000
POLYBACKTEST_REQUEST_INTERVAL_MS=250
POLYBACKTEST_POLL_SECONDS=300
POLYBACKTEST_MAX_RETRIES=5
POLYBACKTEST_RETRY_INITIAL_MS=1500
POLYBACKTEST_ZSTD_LEVEL=3
POLYBACKTEST_INCLUDE_ORDERBOOK=true
POLYBACKTEST_MAX_MARKETS_PER_SYNC=0
```

- Free Plan 必须保持 `POLYBACKTEST_REQUEST_INTERVAL_MS>=1000`；Pro/Enterprise 最低
  100ms。Pro 使用 250ms 时低于 300 requests/minute 的持续限制。
- `POLYBACKTEST_PLAN` 只用于本地限速校验，必须与 API Key 实际套餐一致；程序不会把
  Free Key 伪装成 Pro Key。需要核对时应查询 v4 plan-limits。
- `POLYBACKTEST_COINS=btc,eth,sol` 会在一次 `sync`/`watch` 中依次处理三个资产；
  `POLYBACKTEST_COIN` 继续作为兼容旧配置的单资产回退值。
- `POLYBACKTEST_HISTORY_DAYS=31` 会自动生成“当前时间往前约 31 天”的市场发现边界，
  内置 5 分钟安全余量以适应服务端滚动窗口；固定回补任务可使用显式
  `POLYBACKTEST_HISTORY_START_MS/END_MS`。
  固定回补任务可使用显式 `POLYBACKTEST_HISTORY_START_MS/END_MS`。
- `POLYBACKTEST_MAX_MARKETS_PER_SYNC=0` 表示处理全部待下载市场。设置为正数可用于小批量验证。
- 31 天、三种资产的首次全量补齐耗时取决于盘口快照数量；在 Pro 的 250ms 间隔下，
  通常需要数小时。可先设置 `POLYBACKTEST_MAX_MARKETS_PER_SYNC=1` 验证，再恢复为 `0`。
- Binary Complement Arbitrage 与 Late-Resolution Winner Trading 正式回放应保持
  `POLYBACKTEST_INCLUDE_ORDERBOOK=true`。
- API key 只用于请求头，启动日志不会打印其值。

## 本地布局

```text
data/polybacktest/
├── downloader.lock
├── polybacktest.sqlite3
├── polybacktest.sqlite3-wal
├── polybacktest.sqlite3-shm
└── raw/
    ├── btc/5m/<market_id>/
    ├── eth/5m/<market_id>/
    └── sol/5m/<market_id>/
        ├── market.json.zst
        ├── snapshots-000000000.json.zst
        ├── snapshots-000001000.json.zst
        └── snapshots-000002000.json.zst
```

原始 `.json.zst` 保留 API 返回，方便审计、迁移和重新解析。SQLite 是回测
读取层，启用 WAL、外键和 `synchronous=FULL`。

## SQLite 字段

### `markets`

- ID：`market_id`、`event_id`、`slug`、`condition_id`
- token：`clob_token_up`、`clob_token_down`
- 时间：`start_time`、`end_time`、`resolved_at`
- 结果：`winner`、`coin_price_start`、`coin_price_end`
- 市场统计：`final_volume`、`final_liquidity`
- 下载状态：`status`、`expected_snapshots`、`downloaded_snapshots`、`last_error`

### `snapshots`

- `snapshot_id`、`market_id`、`time`
- `coin_price`
- `coin_twap`、`coin_twap_observed_at`、`coin_twap_source`
- `price_up`、`price_down`
- Up/Down 四个最优档价格与数量
- `orderbook_up_zstd`、`orderbook_down_zstd`：完整盘口 JSON 的 Zstandard BLOB

数值字段以字符串保存，避免把高精度 Chainlink TWAP 提前降为 `f64`。回测时
应解析成 Decimal。

### `pages`

页级恢复清单：`market_id`、`offset`、`count`、原始响应 SHA-256、文件路径和
完成时间。

## 中断与恢复

每页执行顺序：

1. 按 Free Plan 限速请求 API；
2. 将响应压缩写入随机临时文件，`sync_all` 后原子重命名；
3. 在单个 SQLite 事务中写快照、盘口和页清单；
4. 最后一页提交后才把市场标记为 `complete`。

因此：

- 写文件前退出：没有本地变化；
- 原始文件写完但数据库未提交：下次会重新请求该页并安全覆盖；
- 数据库已提交：下次从 `MAX(offset + count)` 继续；
- 同一页重复写入：主键 UPSERT，不产生重复快照；
- 同时启动第二个下载器：`downloader.lock` 会拒绝启动。

## 回测质量门槛

Late-Resolution Winner Trading 严格模型只应使用：

```text
coin_twap IS NOT NULL
coin_twap_source = 'chainlink_twap'
不同 coin_twap_observed_at 才计为独立确认
```

正式回放会按 BTC、ETH、SOL/其他资产分别应用波动率下限和模型风险缓冲，并在一次数据库扫描中运行配置的延迟 × 流动性折扣网格。每轮在
`results/backtests/late-resolution-winner-trading/run-*/` 保存 `run.log`、`report.json` 和 `analysis.zh-CN.md`，旧结果不会被覆盖。

Binary Complement Arbitrage 应从信号快照之后选择首个满足
`time >= signal_time + configured_delay` 的快照成交，逐档读取压缩完整盘口，
并在返回深度不足时判定不成交。不要使用 `snapshot-at` 的最近两秒近似接口。

`winner`、`coin_price_end` 和 `resolved_at` 只能在市场结束后用于结算，不能进入
信号生成，避免未来数据泄漏。

## 控制台输出

运行期间会显示：

- 市场发现数量和本地待下载数量；
- 当前市场和总市场进度条；
- 每页 offset、快照数、原始大小和完成状态；
- 429/5xx/网络异常的退避重试；
- 本轮市场、页、快照、字节和失败汇总。

`watch` 遇到单市场异常会记录 `last_error` 并继续其他市场，下个周期再次恢复。
