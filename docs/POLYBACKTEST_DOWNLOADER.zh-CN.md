# PolyBacktest 历史数据下载器

`polybacktest_downloader` 是独立只读工具，用于持续保存 BTC/ETH/SOL
Up/Down 历史市场，供 Spread Arbitrage 和 Winner Buy 离线回放。它不会读取
Polymarket 私钥，也不会提交订单。

## 运行

```bash
# 下载当前 API 权限范围内所有缺失的已结算市场，然后退出
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

首次使用 Free Plan 下载约 100 个 BTC 5m 市场，完整订单簿通常需要约
20–30 分钟。之后每个新市场约需 3 页和 10–15 秒。

## 配置

```dotenv
POLYBACKTEST_API_KEY=
POLYBACKTEST_API_URL=https://api.polybacktest.com
POLYBACKTEST_COIN=btc
POLYBACKTEST_MARKET_TYPE=5m
POLYBACKTEST_DATA_DIR=data/polybacktest
POLYBACKTEST_DATABASE_FILE=data/polybacktest/polybacktest.sqlite3
POLYBACKTEST_PAGE_LIMIT=1000
POLYBACKTEST_REQUEST_INTERVAL_MS=1100
POLYBACKTEST_POLL_SECONDS=300
POLYBACKTEST_MAX_RETRIES=5
POLYBACKTEST_RETRY_INITIAL_MS=1500
POLYBACKTEST_ZSTD_LEVEL=3
POLYBACKTEST_INCLUDE_ORDERBOOK=true
POLYBACKTEST_MAX_MARKETS_PER_SYNC=0
```

- Free Plan 必须保持 `POLYBACKTEST_REQUEST_INTERVAL_MS>=1000`；默认 1100ms。
- `POLYBACKTEST_MAX_MARKETS_PER_SYNC=0` 表示处理全部待下载市场。设置为正数可用于小批量验证。
- Spread/Winner 正式回放应保持 `POLYBACKTEST_INCLUDE_ORDERBOOK=true`。
- API key 只用于请求头，启动日志不会打印其值。

## 本地布局

```text
data/polybacktest/
├── downloader.lock
├── polybacktest.sqlite3
├── polybacktest.sqlite3-wal
├── polybacktest.sqlite3-shm
└── raw/
    └── btc/5m/<market_id>/
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

Winner Buy 严格模型只应使用：

```text
coin_twap IS NOT NULL
coin_twap_source = 'chainlink_twap'
不同 coin_twap_observed_at 才计为独立确认
```

Spread Arbitrage 应从信号快照之后选择首个满足
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
