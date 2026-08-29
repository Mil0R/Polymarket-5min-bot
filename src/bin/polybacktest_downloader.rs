use anyhow::{bail, Context, Result};
use chrono::Utc;
use fs2::FileExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{StatusCode, Url};
use rusqlite::{params, Connection, Transaction};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SCHEMA_VERSION: i64 = 1;
const HISTORY_WINDOW_SAFETY_MS: i64 = 5 * 60 * 1000;

#[derive(Debug, Clone)]
struct Config {
    api_key: String,
    api_url: String,
    coin: String,
    coins: Vec<String>,
    market_type: String,
    plan: String,
    history_days: u64,
    history_start_ms: Option<i64>,
    history_end_ms: Option<i64>,
    data_dir: PathBuf,
    database_file: PathBuf,
    page_limit: usize,
    request_interval: Duration,
    poll_interval: Duration,
    max_retries: usize,
    retry_initial: Duration,
    compression_level: i32,
    include_orderbook: bool,
    max_markets_per_sync: usize,
}

impl Config {
    fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        let api_key =
            std::env::var("POLYBACKTEST_API_KEY").context("POLYBACKTEST_API_KEY must be set")?;
        if api_key.trim().is_empty() {
            bail!("POLYBACKTEST_API_KEY cannot be empty");
        }
        let data_dir = PathBuf::from(
            std::env::var("POLYBACKTEST_DATA_DIR")
                .unwrap_or_else(|_| "data/polybacktest".to_string()),
        );
        let database_file = std::env::var_os("POLYBACKTEST_DATABASE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("polybacktest.sqlite3"));
        let page_limit = env_parse("POLYBACKTEST_PAGE_LIMIT", 1000usize)?;
        if !(1..=1000).contains(&page_limit) {
            bail!("POLYBACKTEST_PAGE_LIMIT must be in 1..=1000");
        }
        let coins = parse_coins(&std::env::var("POLYBACKTEST_COINS").unwrap_or_else(|_| {
            std::env::var("POLYBACKTEST_COIN").unwrap_or_else(|_| "btc".to_string())
        }))?;
        let coin = coins[0].clone();
        let plan = std::env::var("POLYBACKTEST_PLAN")
            .unwrap_or_else(|_| "free".to_string())
            .trim()
            .to_ascii_lowercase();
        if !matches!(plan.as_str(), "free" | "pro" | "enterprise") {
            bail!("POLYBACKTEST_PLAN must be free, pro, or enterprise");
        }
        let market_type = std::env::var("POLYBACKTEST_MARKET_TYPE")
            .unwrap_or_else(|_| "5m".to_string())
            .trim()
            .to_ascii_lowercase();
        if !matches!(market_type.as_str(), "5m" | "15m" | "1h" | "4h" | "24h") {
            bail!("POLYBACKTEST_MARKET_TYPE must be 5m, 15m, 1h, 4h, or 24h");
        }
        let default_request_interval_ms = if plan == "free" { 1100 } else { 250 };
        let request_interval_ms = env_parse(
            "POLYBACKTEST_REQUEST_INTERVAL_MS",
            default_request_interval_ms,
        )?;
        if plan == "free" && request_interval_ms < 1000 {
            bail!("POLYBACKTEST_REQUEST_INTERVAL_MS must be at least 1000 for Free Plan");
        }
        if plan != "free" && request_interval_ms < 100 {
            bail!("POLYBACKTEST_REQUEST_INTERVAL_MS must be at least 100 for Pro/Enterprise");
        }
        let poll_seconds = env_parse("POLYBACKTEST_POLL_SECONDS", 300u64)?;
        if poll_seconds < 60 {
            bail!("POLYBACKTEST_POLL_SECONDS must be at least 60");
        }
        let max_retries = env_parse("POLYBACKTEST_MAX_RETRIES", 5usize)?;
        if max_retries == 0 {
            bail!("POLYBACKTEST_MAX_RETRIES must be greater than zero");
        }
        let history_days = env_parse("POLYBACKTEST_HISTORY_DAYS", 31u64)?;
        let history_start_ms = env_optional_parse("POLYBACKTEST_HISTORY_START_MS")?;
        let history_end_ms = env_optional_parse("POLYBACKTEST_HISTORY_END_MS")?;
        if let (Some(start), Some(end)) = (history_start_ms, history_end_ms) {
            if start >= end {
                bail!("POLYBACKTEST_HISTORY_START_MS must be less than HISTORY_END_MS");
            }
        }
        Ok(Self {
            api_key,
            api_url: std::env::var("POLYBACKTEST_API_URL")
                .unwrap_or_else(|_| "https://api.polybacktest.com".to_string())
                .trim_end_matches('/')
                .to_string(),
            coin,
            coins,
            market_type,
            plan,
            history_days,
            history_start_ms,
            history_end_ms,
            data_dir,
            database_file,
            page_limit,
            request_interval: Duration::from_millis(request_interval_ms),
            poll_interval: Duration::from_secs(poll_seconds),
            max_retries,
            retry_initial: Duration::from_millis(env_parse(
                "POLYBACKTEST_RETRY_INITIAL_MS",
                1500u64,
            )?),
            compression_level: env_parse("POLYBACKTEST_ZSTD_LEVEL", 3i32)?,
            include_orderbook: env_bool("POLYBACKTEST_INCLUDE_ORDERBOOK", true)?,
            max_markets_per_sync: env_parse("POLYBACKTEST_MAX_MARKETS_PER_SYNC", 0usize)?,
        })
    }

    fn for_coin(&self, coin: &str) -> Self {
        let mut scoped = self.clone();
        scoped.coin = coin.to_string();
        scoped
    }

    fn history_bounds(&self) -> (i64, i64) {
        let end = self.history_end_ms.unwrap_or_else(now_ms);
        let start = self.history_start_ms.unwrap_or_else(|| {
            end.saturating_sub(self.history_days as i64 * 86_400_000)
                .saturating_add(HISTORY_WINDOW_SAFETY_MS)
        });
        (start, end)
    }
}

fn parse_coins(raw: &str) -> Result<Vec<String>> {
    let mut coins = raw
        .split(',')
        .map(|coin| coin.trim().to_ascii_lowercase())
        .filter(|coin| !coin.is_empty())
        .collect::<Vec<_>>();
    if coins.is_empty()
        || coins
            .iter()
            .any(|coin| !matches!(coin.as_str(), "btc" | "eth" | "sol"))
    {
        bail!("POLYBACKTEST_COINS must contain one or more of btc, eth, sol");
    }
    coins.dedup();
    Ok(coins)
}

fn env_parse<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr + std::fmt::Display,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    std::env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .with_context(|| format!("{name} has an invalid value"))
}

fn env_optional_parse<T>(name: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value
            .parse()
            .map(Some)
            .with_context(|| format!("{name} has an invalid value")),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn env_bool(name: &str, default: bool) -> Result<bool> {
    match std::env::var(name) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => bail!("{name} must be true or false"),
        },
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug)]
struct ApiClient {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    request_interval: Duration,
    last_request_started: Option<Instant>,
    max_retries: usize,
    retry_initial: Duration,
}

impl ApiClient {
    fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .user_agent(concat!(
                    "polypulse-polybacktest/",
                    env!("CARGO_PKG_VERSION")
                ))
                .build()?,
            api_key: config.api_key.clone(),
            base_url: config.api_url.clone(),
            request_interval: config.request_interval,
            last_request_started: None,
            max_retries: config.max_retries,
            retry_initial: config.retry_initial,
        })
    }

    async fn get(&mut self, path: &str, query: &[(&str, String)]) -> Result<Vec<u8>> {
        let mut retry_delay = self.retry_initial;
        for attempt in 1..=self.max_retries {
            self.wait_for_rate_limit().await;
            let url = Url::parse(&format!("{}{}", self.base_url, path))?;
            let response = self
                .client
                .get(url)
                .query(query)
                .header("X-API-Key", &self.api_key)
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    return Ok(response.bytes().await?.to_vec());
                }
                Ok(response) => {
                    let status = response.status();
                    let retry_after = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let body = response.text().await.unwrap_or_default();
                    let summary: String = body.chars().take(500).collect();
                    if attempt < self.max_retries && retryable_status(status) {
                        let delay = retry_after.unwrap_or(retry_delay);
                        log_line(
                            "WARN",
                            format!(
                                "HTTP {status}; retry {attempt}/{} in {:.1}s: {summary}",
                                self.max_retries,
                                delay.as_secs_f64()
                            ),
                        );
                        tokio::time::sleep(delay).await;
                        retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(30));
                        continue;
                    }
                    bail!("PolyBacktest HTTP {status}: {summary}");
                }
                Err(error) if attempt < self.max_retries => {
                    log_line(
                        "WARN",
                        format!(
                            "request failed; retry {attempt}/{} in {:.1}s: {error}",
                            self.max_retries,
                            retry_delay.as_secs_f64()
                        ),
                    );
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(30));
                }
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("retry loop always returns")
    }

    async fn wait_for_rate_limit(&mut self) {
        if let Some(last) = self.last_request_started {
            let elapsed = last.elapsed();
            if elapsed < self.request_interval {
                tokio::time::sleep(self.request_interval - elapsed).await;
            }
        }
        self.last_request_started = Some(Instant::now());
    }
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

#[derive(Debug, Clone)]
struct MarketRecord {
    market_id: String,
    raw: Value,
}

#[derive(Debug)]
struct SnapshotPage {
    raw: Vec<u8>,
    value: Value,
    count: usize,
    has_more: bool,
    next_offset: Option<usize>,
    total: Option<usize>,
}

impl SnapshotPage {
    fn parse(raw: Vec<u8>) -> Result<Self> {
        let value: Value = serde_json::from_slice(&raw).context("invalid snapshot page JSON")?;
        let snapshots = value
            .get("snapshots")
            .and_then(Value::as_array)
            .context("snapshot response is missing snapshots")?;
        Ok(Self {
            count: snapshots.len(),
            has_more: value
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            next_offset: value
                .get("next_offset")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            total: value
                .get("total")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            raw,
            value,
        })
    }
}

#[derive(Debug)]
struct Store {
    connection: Connection,
    data_dir: PathBuf,
    compression_level: i32,
}

impl Store {
    fn open(config: &Config) -> Result<Self> {
        if let Some(parent) = config.database_file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir_all(&config.data_dir)?;
        let connection = Connection::open(&config.database_file)
            .with_context(|| format!("cannot open {}", config.database_file.display()))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )?;
        initialize_schema(&connection)?;
        Ok(Self {
            connection,
            data_dir: config.data_dir.clone(),
            compression_level: config.compression_level,
        })
    }

    fn upsert_market(&mut self, coin: &str, market_type: &str, market: &Value) -> Result<String> {
        let market_id = text_field(market, "market_id")?;
        let raw_path = self.market_raw_path(coin, market_type, &market_id);
        write_zstd_atomic(
            &raw_path,
            &serde_json::to_vec(market)?,
            self.compression_level,
        )?;
        self.connection.execute(
            "INSERT INTO markets (
                market_id, coin, market_type, event_id, slug, condition_id,
                clob_token_up, clob_token_down, start_time, end_time, winner,
                coin_price_start, coin_price_end, final_volume, final_liquidity,
                resolved_at, status, raw_market_path, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,
                       COALESCE((SELECT status FROM markets WHERE market_id=?1),'discovered'),?17,?18)
             ON CONFLICT(market_id) DO UPDATE SET
                event_id=excluded.event_id, slug=excluded.slug,
                condition_id=excluded.condition_id, clob_token_up=excluded.clob_token_up,
                clob_token_down=excluded.clob_token_down, start_time=excluded.start_time,
                end_time=excluded.end_time, winner=excluded.winner,
                coin_price_start=excluded.coin_price_start, coin_price_end=excluded.coin_price_end,
                final_volume=excluded.final_volume, final_liquidity=excluded.final_liquidity,
                resolved_at=excluded.resolved_at, raw_market_path=excluded.raw_market_path,
                updated_at=excluded.updated_at",
            params![
                market_id,
                coin,
                market_type,
                optional_text(market, "event_id"),
                optional_text(market, "slug"),
                optional_text(market, "condition_id"),
                optional_text(market, "clob_token_up"),
                optional_text(market, "clob_token_down"),
                optional_i64(market, "start_time"),
                optional_i64(market, "end_time"),
                optional_text(market, "winner"),
                dynamic_text(market, coin, "price_start"),
                dynamic_text(market, coin, "price_end"),
                scalar_text(market.get("final_volume")),
                scalar_text(market.get("final_liquidity")),
                optional_i64(market, "resolved_at"),
                raw_path.to_string_lossy(),
                now_ms(),
            ],
        )?;
        Ok(market_id)
    }

    fn pending_markets(&self, coin: &str, market_type: &str) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT market_id FROM markets
             WHERE coin=?1 AND market_type=?2 AND status!='complete'
             ORDER BY start_time ASC, market_id ASC",
        )?;
        let markets = statement
            .query_map(params![coin, market_type], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(markets)
    }

    fn resume_offset(&self, market_id: &str) -> Result<usize> {
        let offset: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(offset + count), 0) FROM pages WHERE market_id=?1",
            [market_id],
            |row| row.get(0),
        )?;
        Ok(offset.max(0) as usize)
    }

    fn persist_page(
        &mut self,
        coin: &str,
        market_type: &str,
        market_id: &str,
        offset: usize,
        page: &SnapshotPage,
    ) -> Result<()> {
        let raw_path = self.page_raw_path(coin, market_type, market_id, offset);
        write_zstd_atomic(&raw_path, &page.raw, self.compression_level)?;
        let sha256 = format!("{:x}", Sha256::digest(&page.raw));
        let transaction = self.connection.transaction()?;
        persist_snapshots(&transaction, coin, market_id, &page.value)?;
        transaction.execute(
            "INSERT INTO pages (market_id, offset, count, sha256, raw_path, completed_at)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(market_id,offset) DO UPDATE SET count=excluded.count,
                sha256=excluded.sha256, raw_path=excluded.raw_path,
                completed_at=excluded.completed_at",
            params![
                market_id,
                offset as i64,
                page.count as i64,
                sha256,
                raw_path.to_string_lossy(),
                now_ms()
            ],
        )?;
        let downloaded: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM snapshots WHERE market_id=?1",
            [market_id],
            |row| row.get(0),
        )?;
        let status = if page.has_more {
            "downloading"
        } else {
            "complete"
        };
        transaction.execute(
            "UPDATE markets SET status=?2, downloaded_snapshots=?3,
                expected_snapshots=COALESCE(?4,expected_snapshots), last_error=NULL,
                updated_at=?5 WHERE market_id=?1",
            params![
                market_id,
                status,
                downloaded,
                page.total.map(|value| value as i64),
                now_ms()
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn record_error(&self, market_id: &str, error: &anyhow::Error) -> Result<()> {
        self.connection.execute(
            "UPDATE markets SET status='error', last_error=?2, updated_at=?3 WHERE market_id=?1",
            params![market_id, format!("{error:#}"), now_ms()],
        )?;
        Ok(())
    }

    fn status(&self, coin: &str, market_type: &str) -> Result<LocalStatus> {
        let (markets, complete, snapshots, book_snapshots, pages): (i64, i64, i64, i64, i64) =
            self.connection.query_row(
                "SELECT
                   COUNT(*), COALESCE(SUM(CASE WHEN status='complete' THEN 1 ELSE 0 END),0),
                   COALESCE((SELECT COUNT(*) FROM snapshots s JOIN markets m ON m.market_id=s.market_id WHERE m.coin=?1 AND m.market_type=?2),0),
                   COALESCE((SELECT COUNT(*) FROM snapshots s JOIN markets m ON m.market_id=s.market_id WHERE m.coin=?1 AND m.market_type=?2 AND s.orderbook_up_zstd IS NOT NULL AND s.orderbook_down_zstd IS NOT NULL),0),
                   COALESCE((SELECT COUNT(*) FROM pages p JOIN markets m ON m.market_id=p.market_id WHERE m.coin=?1 AND m.market_type=?2),0)
                 FROM markets WHERE coin=?1 AND market_type=?2",
                params![coin, market_type],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )?;
        Ok(LocalStatus {
            markets,
            complete,
            snapshots,
            book_snapshots,
            pages,
        })
    }

    fn market_raw_path(&self, coin: &str, market_type: &str, market_id: &str) -> PathBuf {
        self.data_dir
            .join("raw")
            .join(coin)
            .join(market_type)
            .join(market_id)
            .join("market.json.zst")
    }

    fn page_raw_path(
        &self,
        coin: &str,
        market_type: &str,
        market_id: &str,
        offset: usize,
    ) -> PathBuf {
        self.data_dir
            .join("raw")
            .join(coin)
            .join(market_type)
            .join(market_id)
            .join(format!("snapshots-{offset:09}.json.zst"))
    }
}

#[derive(Debug)]
struct LocalStatus {
    markets: i64,
    complete: i64,
    snapshots: i64,
    book_snapshots: i64,
    pages: i64,
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    let current: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        bail!("database schema {current} is newer than supported {SCHEMA_VERSION}");
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS markets (
           market_id TEXT PRIMARY KEY,
           coin TEXT NOT NULL,
           market_type TEXT NOT NULL,
           event_id TEXT,
           slug TEXT,
           condition_id TEXT,
           clob_token_up TEXT,
           clob_token_down TEXT,
           start_time INTEGER,
           end_time INTEGER,
           winner TEXT,
           coin_price_start TEXT,
           coin_price_end TEXT,
           final_volume TEXT,
           final_liquidity TEXT,
           resolved_at INTEGER,
           status TEXT NOT NULL DEFAULT 'discovered',
           expected_snapshots INTEGER,
           downloaded_snapshots INTEGER NOT NULL DEFAULT 0,
           last_error TEXT,
           raw_market_path TEXT,
           updated_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS markets_scope_time
           ON markets(coin,market_type,start_time);
         CREATE TABLE IF NOT EXISTS snapshots (
           market_id TEXT NOT NULL REFERENCES markets(market_id) ON DELETE CASCADE,
           snapshot_id INTEGER NOT NULL,
           time INTEGER NOT NULL,
           coin_price TEXT,
           coin_twap TEXT,
           coin_twap_observed_at INTEGER,
           coin_twap_source TEXT,
           price_up TEXT,
           price_down TEXT,
           up_best_bid TEXT,
           up_best_bid_size TEXT,
           up_best_ask TEXT,
           up_best_ask_size TEXT,
           down_best_bid TEXT,
           down_best_bid_size TEXT,
           down_best_ask TEXT,
           down_best_ask_size TEXT,
           orderbook_up_zstd BLOB,
           orderbook_down_zstd BLOB,
           PRIMARY KEY(market_id,snapshot_id)
         );
         CREATE INDEX IF NOT EXISTS snapshots_market_time ON snapshots(market_id,time);
         CREATE TABLE IF NOT EXISTS pages (
           market_id TEXT NOT NULL REFERENCES markets(market_id) ON DELETE CASCADE,
           offset INTEGER NOT NULL,
           count INTEGER NOT NULL,
           sha256 TEXT NOT NULL,
           raw_path TEXT NOT NULL,
           completed_at INTEGER NOT NULL,
           PRIMARY KEY(market_id,offset)
         );",
    )?;
    connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn persist_snapshots(
    transaction: &Transaction<'_>,
    coin: &str,
    market_id: &str,
    response: &Value,
) -> Result<()> {
    let snapshots = response
        .get("snapshots")
        .and_then(Value::as_array)
        .context("snapshot response is missing snapshots")?;
    let mut snapshot_statement = transaction.prepare_cached(
        "INSERT INTO snapshots (
           market_id,snapshot_id,time,coin_price,coin_twap,coin_twap_observed_at,
           coin_twap_source,price_up,price_down,
           up_best_bid,up_best_bid_size,up_best_ask,up_best_ask_size,
           down_best_bid,down_best_bid_size,down_best_ask,down_best_ask_size,
           orderbook_up_zstd,orderbook_down_zstd
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
         ON CONFLICT(market_id,snapshot_id) DO UPDATE SET
           time=excluded.time, coin_price=excluded.coin_price,
           coin_twap=excluded.coin_twap,
           coin_twap_observed_at=excluded.coin_twap_observed_at,
           coin_twap_source=excluded.coin_twap_source,
           price_up=excluded.price_up, price_down=excluded.price_down,
           up_best_bid=excluded.up_best_bid,up_best_bid_size=excluded.up_best_bid_size,
           up_best_ask=excluded.up_best_ask,up_best_ask_size=excluded.up_best_ask_size,
           down_best_bid=excluded.down_best_bid,down_best_bid_size=excluded.down_best_bid_size,
           down_best_ask=excluded.down_best_ask,down_best_ask_size=excluded.down_best_ask_size,
           orderbook_up_zstd=excluded.orderbook_up_zstd,
           orderbook_down_zstd=excluded.orderbook_down_zstd",
    )?;
    for snapshot in snapshots {
        let snapshot_id = required_i64(snapshot, "id")?;
        let (up_best_bid, up_best_bid_size) = top_entry(snapshot, "orderbook_up", "bids");
        let (up_best_ask, up_best_ask_size) = top_entry(snapshot, "orderbook_up", "asks");
        let (down_best_bid, down_best_bid_size) = top_entry(snapshot, "orderbook_down", "bids");
        let (down_best_ask, down_best_ask_size) = top_entry(snapshot, "orderbook_down", "asks");
        snapshot_statement.execute(params![
            market_id,
            snapshot_id,
            required_i64(snapshot, "time")?,
            dynamic_text(snapshot, coin, "price"),
            dynamic_text(snapshot, coin, "twap"),
            dynamic_i64(snapshot, coin, "twap_observed_at"),
            dynamic_text(snapshot, coin, "twap_source"),
            scalar_text(snapshot.get("price_up")),
            scalar_text(snapshot.get("price_down")),
            up_best_bid,
            up_best_bid_size,
            up_best_ask,
            up_best_ask_size,
            down_best_bid,
            down_best_bid_size,
            down_best_ask,
            down_best_ask_size,
            compress_json(snapshot.get("orderbook_up"))?,
            compress_json(snapshot.get("orderbook_down"))?,
        ])?;
    }
    Ok(())
}

fn top_entry(snapshot: &Value, book: &str, side: &str) -> (Option<String>, Option<String>) {
    let entry = snapshot
        .get(book)
        .and_then(|book| book.get(side))
        .and_then(Value::as_array)
        .and_then(|entries| entries.first());
    (
        scalar_text(entry.and_then(|entry| entry.get("price"))),
        scalar_text(entry.and_then(|entry| entry.get("size"))),
    )
}

fn compress_json(value: Option<&Value>) -> Result<Option<Vec<u8>>> {
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            let bytes = serde_json::to_vec(value)?;
            Ok(zstd::stream::encode_all(bytes.as_slice(), 1)?)
        })
        .transpose()
}

#[derive(Debug)]
struct Downloader {
    config: Config,
    api: ApiClient,
    store: Store,
    _runtime_lock: File,
}

impl Downloader {
    fn new(config: Config) -> Result<Self> {
        let runtime_lock = acquire_runtime_lock(&config)?;
        let api = ApiClient::new(&config)?;
        let store = Store::open(&config)?;
        Ok(Self {
            config,
            api,
            store,
            _runtime_lock: runtime_lock,
        })
    }

    async fn discover_markets(&mut self) -> Result<Vec<MarketRecord>> {
        let mut offset = 0usize;
        let mut markets = Vec::new();
        let (history_start_ms, history_end_ms) = self.config.history_bounds();
        loop {
            let raw = self
                .api
                .get(
                    "/v4/markets",
                    &[
                        ("coin", self.config.coin.clone()),
                        ("market_type", self.config.market_type.clone()),
                        ("resolved", "true".to_string()),
                        ("start_time", history_start_ms.to_string()),
                        ("end_time", history_end_ms.to_string()),
                        ("limit", "100".to_string()),
                        ("offset", offset.to_string()),
                        ("include_total", "true".to_string()),
                    ],
                )
                .await?;
            let response: Value = serde_json::from_slice(&raw)?;
            let page = response
                .get("markets")
                .and_then(Value::as_array)
                .context("market response is missing markets")?;
            for market in page {
                markets.push(MarketRecord {
                    market_id: text_field(market, "market_id")?,
                    raw: market.clone(),
                });
            }
            if !response
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                if let Some(warning) = response.get("warning").and_then(Value::as_str) {
                    log_line("WARN", warning);
                }
                break;
            }
            offset = response
                .get("next_offset")
                .and_then(Value::as_u64)
                .context("market response has_more without next_offset")?
                as usize;
        }
        Ok(markets)
    }

    async fn sync_once(&mut self) -> Result<SyncSummary> {
        log_line(
            "INFO",
            format!(
                "discovering resolved {} {} markets in {}..{} (plan={})",
                self.config.coin.to_ascii_uppercase(),
                self.config.market_type,
                self.config.history_bounds().0,
                self.config.history_bounds().1,
                self.config.plan
            ),
        );
        let discovered = self.discover_markets().await?;
        for market in &discovered {
            let stored = self.store.upsert_market(
                &self.config.coin,
                &self.config.market_type,
                &market.raw,
            )?;
            debug_assert_eq!(stored, market.market_id);
        }
        let mut pending = self
            .store
            .pending_markets(&self.config.coin, &self.config.market_type)?;
        if self.config.max_markets_per_sync > 0 {
            pending.truncate(self.config.max_markets_per_sync);
        }
        log_line(
            "INFO",
            format!(
                "discovered={} pending={} local_database={}",
                discovered.len(),
                pending.len(),
                self.config.database_file.display()
            ),
        );
        let progress = ProgressBar::new(pending.len() as u64);
        progress.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:36.cyan/blue}] {pos}/{len} {msg}",
            )?
            .progress_chars("=>-"),
        );
        let mut summary = SyncSummary {
            discovered: discovered.len(),
            ..SyncSummary::default()
        };
        let pending_count = pending.len();
        let sync_started = Instant::now();
        for (market_index, market_id) in pending.into_iter().enumerate() {
            progress.set_message(format!("market {market_id}"));
            log_line(
                "INFO",
                format!(
                    "market_progress={}/{} market={} action=download",
                    market_index + 1,
                    pending_count,
                    market_id
                ),
            );
            match self.download_market(&market_id).await {
                Ok((pages, snapshots, bytes)) => {
                    summary.completed += 1;
                    summary.pages += pages;
                    summary.snapshots += snapshots;
                    summary.bytes += bytes;
                }
                Err(error) => {
                    summary.failed += 1;
                    self.store.record_error(&market_id, &error)?;
                    log_line("ERROR", format!("market {market_id}: {error:#}"));
                }
            }
            progress.inc(1);
            let processed = market_index + 1;
            let elapsed = sync_started.elapsed();
            let eta = if processed < pending_count {
                Duration::from_secs_f64(
                    elapsed.as_secs_f64() / processed as f64 * (pending_count - processed) as f64,
                )
            } else {
                Duration::ZERO
            };
            log_line(
                "INFO",
                format!(
                    "overall_progress={processed}/{pending_count} ({:.1}%) elapsed={}s eta={}s completed={} failed={} snapshots={} raw={}",
                    processed as f64 / pending_count as f64 * 100.0,
                    elapsed.as_secs(),
                    eta.as_secs(),
                    summary.completed,
                    summary.failed,
                    summary.snapshots,
                    human_bytes(summary.bytes)
                ),
            );
        }
        progress.finish_and_clear();
        Ok(summary)
    }

    async fn download_market(&mut self, market_id: &str) -> Result<(usize, usize, u64)> {
        let mut offset = self.store.resume_offset(market_id)?;
        log_line("INFO", format!("market={market_id} resume_offset={offset}"));
        let mut pages = 0usize;
        let mut snapshots = 0usize;
        let mut bytes = 0u64;
        let mut market_total = None;
        loop {
            let page_bytes = self
                .api
                .get(
                    &format!("/v4/markets/{market_id}/snapshots"),
                    &[
                        ("coin", self.config.coin.clone()),
                        ("limit", self.config.page_limit.to_string()),
                        ("offset", offset.to_string()),
                        (
                            "include_orderbook",
                            self.config.include_orderbook.to_string(),
                        ),
                        ("include_total", (pages == 0).to_string()),
                    ],
                )
                .await?;
            let page = SnapshotPage::parse(page_bytes)?;
            if page.total.is_some() {
                market_total = page.total;
            }
            if page.count == 0 && page.has_more {
                bail!("empty snapshot page at offset {offset} with has_more=true");
            }
            self.store.persist_page(
                &self.config.coin,
                &self.config.market_type,
                market_id,
                offset,
                &page,
            )?;
            pages += 1;
            snapshots += page.count;
            bytes += page.raw.len() as u64;
            let downloaded = offset.saturating_add(page.count);
            let page_progress = market_total
                .filter(|total| *total > 0)
                .map(|total| {
                    format!(
                        "{downloaded}/{total} ({:.1}%)",
                        downloaded as f64 / total as f64 * 100.0
                    )
                })
                .unwrap_or_else(|| downloaded.to_string());
            log_line(
                "INFO",
                format!(
                    "market={} offset={} page_snapshots={} snapshot_progress={} raw={} complete={}",
                    market_id,
                    offset,
                    page.count,
                    page_progress,
                    human_bytes(page.raw.len() as u64),
                    !page.has_more
                ),
            );
            if !page.has_more {
                break;
            }
            offset = page
                .next_offset
                .unwrap_or_else(|| offset.saturating_add(page.count));
        }
        Ok((pages, snapshots, bytes))
    }
}

#[derive(Debug, Default)]
struct SyncSummary {
    discovered: usize,
    completed: usize,
    failed: usize,
    pages: usize,
    snapshots: usize,
    bytes: u64,
}

fn required_i64(value: &Value, field: &str) -> Result<i64> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .with_context(|| format!("missing integer field {field}"))
}

fn optional_i64(value: &Value, field: &str) -> Option<i64> {
    value.get(field).and_then(Value::as_i64)
}

fn dynamic_i64(value: &Value, prefix: &str, suffix: &str) -> Option<i64> {
    value
        .get(format!("{prefix}_{suffix}"))
        .and_then(Value::as_i64)
}

fn text_field(value: &Value, field: &str) -> Result<String> {
    optional_text(value, field).with_context(|| format!("missing text field {field}"))
}

fn optional_text(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn dynamic_text(value: &Value, prefix: &str, suffix: &str) -> Option<String> {
    scalar_text(value.get(format!("{prefix}_{suffix}")))
}

fn scalar_text(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    })
}

fn write_zstd_atomic(path: &Path, bytes: &[u8], level: i32) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("page"),
        uuid::Uuid::new_v4()
    ));
    let file = File::create(&temporary)?;
    let mut encoder = zstd::stream::write::Encoder::new(file, level)?;
    encoder.write_all(bytes)?;
    let file = encoder.finish()?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn acquire_runtime_lock(config: &Config) -> Result<File> {
    fs::create_dir_all(&config.data_dir)?;
    let path = config.data_dir.join("downloader.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    file.try_lock_exclusive().with_context(|| {
        format!(
            "another PolyBacktest downloader is using {}",
            path.display()
        )
    })?;
    Ok(file)
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn log_line(level: &str, message: impl std::fmt::Display) {
    println!("{} {level:<5} {message}", timestamp());
}

fn human_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.2}MiB", bytes as f64 / MIB)
    } else if bytes >= 1024 {
        format!("{:.1}KiB", bytes as f64 / KIB)
    } else {
        format!("{bytes}B")
    }
}

fn print_status(status: &LocalStatus, config: &Config) {
    println!("PolyBacktest local status");
    println!(
        "  scope:       {} {}",
        config.coin.to_ascii_uppercase(),
        config.market_type
    );
    println!("  database:    {}", config.database_file.display());
    println!("  markets:     {}", status.markets);
    println!("  complete:    {}", status.complete);
    println!("  snapshots:   {}", status.snapshots);
    println!("  full books:  {} snapshots", status.book_snapshots);
    println!("  pages:       {}", status.pages);
}

async fn run_sync(downloader: &mut Downloader) -> Result<()> {
    let summary = downloader.sync_once().await?;
    log_line(
        "INFO",
        format!(
            "sync finished discovered={} completed={} failed={} pages={} snapshots={} raw={}",
            summary.discovered,
            summary.completed,
            summary.failed,
            summary.pages,
            summary.snapshots,
            human_bytes(summary.bytes)
        ),
    );
    if summary.failed > 0 {
        bail!(
            "{} market downloads failed; rerun to resume",
            summary.failed
        );
    }
    Ok(())
}

async fn run_sync_all(config: &Config) -> Result<()> {
    let mut failures = 0usize;
    for coin in &config.coins {
        let scoped_config = config.for_coin(coin);
        log_line(
            "INFO",
            format!("starting sync scope={}", coin.to_ascii_uppercase()),
        );
        match Downloader::new(scoped_config) {
            Ok(mut downloader) => {
                if let Err(error) = run_sync(&mut downloader).await {
                    failures += 1;
                    log_line("ERROR", format!("sync scope={} failed: {error:#}", coin));
                }
            }
            Err(error) => {
                failures += 1;
                log_line(
                    "ERROR",
                    format!("initializing scope={} failed: {error:#}", coin),
                );
            }
        }
    }
    if failures > 0 {
        bail!("{} asset sync scope(s) failed; rerun to resume", failures);
    }
    Ok(())
}

async fn run_watch_all(config: Config) -> Result<()> {
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            result = run_sync_all(&config) => {
                if let Err(error) = result {
                    log_line("ERROR", format!("sync cycle failed: {error:#}"));
                }
            }
            result = &mut shutdown => {
                result?;
                log_line("INFO", "shutdown requested; committed pages are safe");
                return Ok(());
            }
        }
        log_line(
            "INFO",
            format!(
                "next update in {} seconds for {}",
                config.poll_interval.as_secs(),
                config.coins.join(",")
            ),
        );
        tokio::select! {
            _ = tokio::time::sleep(config.poll_interval) => {}
            result = &mut shutdown => {
                result?;
                log_line("INFO", "shutdown requested");
                return Ok(());
            }
        }
    }
}

fn print_usage() {
    println!(
        "Usage: polybacktest_downloader [sync|watch|status]\n\
         \n\
         sync    Download missing resolved markets for all configured coins once (default)\n\
         watch   Keep polling and downloading new resolved markets\n\
         status  Show local database counts without making API requests"
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "sync".to_string());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        print_usage();
        return Ok(());
    }
    let config = Config::from_env()?;
    log_line(
        "INFO",
        format!(
            "PolyBacktest downloader mode={} scopes={} market_type={} history_days={} plan={} page_limit={} orderbook={} request_interval_ms={} (API key hidden)",
            command,
            config
                .coins
                .iter()
                .map(|coin| coin.to_ascii_uppercase())
                .collect::<Vec<_>>()
                .join(","),
            config.market_type,
            config.history_days,
            config.plan,
            config.page_limit,
            config.include_orderbook,
            config.request_interval.as_millis()
        ),
    );
    match command.as_str() {
        "sync" => {
            let shutdown = tokio::signal::ctrl_c();
            tokio::pin!(shutdown);
            tokio::select! {
                result = run_sync_all(&config) => result,
                result = &mut shutdown => {
                    result?;
                    log_line("INFO", "shutdown requested; rerun sync to resume");
                    Ok(())
                }
            }
        }
        "watch" => run_watch_all(config).await,
        "status" => {
            for coin in &config.coins {
                let scoped_config = config.for_coin(coin);
                let store = Store::open(&scoped_config)?;
                print_status(&store.status(coin, &config.market_type)?, &scoped_config);
            }
            Ok(())
        }
        _ => {
            print_usage();
            bail!("unknown command {command}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn test_config(root: &Path) -> Config {
        Config {
            api_key: "hidden".to_string(),
            api_url: "https://example.invalid".to_string(),
            coin: "btc".to_string(),
            coins: vec!["btc".to_string()],
            market_type: "5m".to_string(),
            plan: "free".to_string(),
            history_days: 31,
            history_start_ms: None,
            history_end_ms: None,
            data_dir: root.to_path_buf(),
            database_file: root.join("catalog.sqlite3"),
            page_limit: 1000,
            request_interval: Duration::from_secs(1),
            poll_interval: Duration::from_secs(300),
            max_retries: 3,
            retry_initial: Duration::ZERO,
            compression_level: 1,
            include_orderbook: true,
            max_markets_per_sync: 0,
        }
    }

    fn market() -> Value {
        serde_json::json!({
            "market_id":"42", "event_id":"7", "slug":"btc-updown-5m-test",
            "market_type":"5m", "start_time":1000, "end_time":301000,
            "btc_price_start":100.0, "btc_price_end":101.0,
            "winner":"Up", "resolved_at":302000
        })
    }

    fn page(id: i64, has_more: bool, next_offset: Option<usize>) -> SnapshotPage {
        let value = serde_json::json!({
            "snapshots":[{
                "id":id, "market_id":"42", "time":1000 + id,
                "btc_price":100.1, "btc_twap":"100.05",
                "btc_twap_observed_at":1000, "btc_twap_source":"chainlink_twap",
                "price_up":0.51, "price_down":0.50,
                "orderbook_up":{"asks":[{"price":0.51,"size":10}],"bids":[{"price":0.50,"size":9}]},
                "orderbook_down":{"asks":[{"price":0.50,"size":9}],"bids":[{"price":0.49,"size":10}]}
            }],
            "total":1, "has_more":has_more, "next_offset":next_offset
        });
        SnapshotPage::parse(serde_json::to_vec(&value).unwrap()).unwrap()
    }

    #[test]
    fn persisted_pages_are_idempotent_and_resumable() {
        let root = std::env::temp_dir().join(format!(
            "polypulse-polybacktest-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config = test_config(&root);
        let mut store = Store::open(&config).unwrap();
        store.upsert_market("btc", "5m", &market()).unwrap();
        let initial_page = page(11, true, Some(1));
        store
            .persist_page("btc", "5m", "42", 0, &initial_page)
            .unwrap();
        store
            .persist_page("btc", "5m", "42", 0, &initial_page)
            .unwrap();
        assert_eq!(store.resume_offset("42").unwrap(), 1);
        let snapshot_count: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
            .unwrap();
        let book_count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM snapshots WHERE orderbook_up_zstd IS NOT NULL AND orderbook_down_zstd IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(snapshot_count, 1);
        assert_eq!(book_count, 1);
        let (best_up_ask, compressed): (String, Vec<u8>) = store
            .connection
            .query_row(
                "SELECT up_best_ask,orderbook_up_zstd FROM snapshots",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(best_up_ask, "0.51");
        let decoded = zstd::stream::decode_all(compressed.as_slice()).unwrap();
        let book: Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(book["asks"][0]["size"], 10);

        let final_page = page(12, false, None);
        store
            .persist_page("btc", "5m", "42", 1, &final_page)
            .unwrap();
        let status: String = store
            .connection
            .query_row(
                "SELECT status FROM markets WHERE market_id='42'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "complete");
        drop(store);

        let reopened = Store::open(&config).unwrap();
        assert!(reopened.pending_markets("btc", "5m").unwrap().is_empty());
        let snapshot_count: i64 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(snapshot_count, 2);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compressed_raw_page_round_trips() {
        let root = std::env::temp_dir().join(format!(
            "polypulse-polybacktest-zstd-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("page.json.zst");
        let expected = br#"{"snapshots":[]}"#;
        write_zstd_atomic(&path, expected, 1).unwrap();
        let mut decoder = zstd::stream::read::Decoder::new(File::open(path).unwrap()).unwrap();
        let mut actual = Vec::new();
        decoder.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, expected);
        drop(decoder);
        std::fs::remove_dir_all(root).unwrap();
    }
}
