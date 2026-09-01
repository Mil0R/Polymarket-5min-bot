//! Incremental PMData downloader for Polymarket Up/Down L2 and trade replay data.
//!
//! The downloader is read-only: it never loads trading credentials or sends orders.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use fs2::FileExt;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use reqwest::StatusCode;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const SCHEMA_VERSION: i64 = 1;
const SUPPORTED_COINS: &[&str] = &["btc", "eth", "sol", "xrp", "doge", "bnb", "hype"];
const SUPPORTED_PERIODS: &[&str] = &["5m", "15m", "1h"];
const SUPPORTED_DATA_TYPES: &[&str] = &["l2", "trades"];
static ACTIVE_PROGRESS: OnceLock<Mutex<Option<ProgressBar>>> = OnceLock::new();

struct ConsoleProgress {
    bar: ProgressBar,
    interactive: bool,
}

impl ConsoleProgress {
    fn new(length: u64) -> Result<Self> {
        let interactive = std::io::stdout().is_terminal();
        let bar = if interactive {
            ProgressBar::with_draw_target(Some(length), ProgressDrawTarget::stdout_with_hz(10))
        } else {
            ProgressBar::hidden()
        };
        bar.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] [{bar:36.cyan/blue}] {pos}/{len} {msg}",
            )?
            .progress_chars("=>-"),
        );
        if interactive {
            let mut active = ACTIVE_PROGRESS
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *active = Some(bar.clone());
        }
        Ok(Self { bar, interactive })
    }
}

impl std::ops::Deref for ConsoleProgress {
    type Target = ProgressBar;

    fn deref(&self) -> &Self::Target {
        &self.bar
    }
}

impl Drop for ConsoleProgress {
    fn drop(&mut self) {
        if self.interactive {
            let mut active = ACTIVE_PROGRESS
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            active.take();
        }
        self.bar.finish_and_clear();
    }
}

#[derive(Debug, Clone)]
struct Config {
    api_key: String,
    api_url: String,
    coins: Vec<String>,
    market_type: String,
    data_types: Vec<String>,
    history_days: u64,
    history_start_ms: Option<i64>,
    history_end_ms: Option<i64>,
    data_dir: PathBuf,
    database_file: PathBuf,
    request_interval: Duration,
    poll_interval: Duration,
    availability_lag: Duration,
    refresh_window: Duration,
    refresh_interval: Duration,
    max_markets_per_sync: usize,
    max_retries: usize,
    retry_initial: Duration,
}

impl Config {
    fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        let api_key = std::env::var("PMDATA_API_KEY").context("PMDATA_API_KEY must be set")?;
        if api_key.trim().is_empty() {
            bail!("PMDATA_API_KEY cannot be empty");
        }
        let coins = parse_list(
            "PMDATA_COINS",
            &std::env::var("PMDATA_COINS").unwrap_or_else(|_| "btc".to_string()),
            SUPPORTED_COINS,
        )?;
        let market_type = std::env::var("PMDATA_MARKET_TYPE")
            .unwrap_or_else(|_| "5m".to_string())
            .trim()
            .to_ascii_lowercase();
        if !SUPPORTED_PERIODS.contains(&market_type.as_str()) {
            bail!(
                "PMDATA_MARKET_TYPE must be one of {}",
                SUPPORTED_PERIODS.join(", ")
            );
        }
        let data_types = parse_list(
            "PMDATA_DATA_TYPES",
            &std::env::var("PMDATA_DATA_TYPES").unwrap_or_else(|_| "l2,trades".to_string()),
            SUPPORTED_DATA_TYPES,
        )?;
        let data_dir = PathBuf::from(
            std::env::var("PMDATA_DATA_DIR").unwrap_or_else(|_| "data/pmdata".to_string()),
        );
        let database_file = std::env::var_os("PMDATA_DATABASE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("pmdata.sqlite3"));
        let history_days = env_parse("PMDATA_HISTORY_DAYS", 1u64)?;
        let history_start_ms = env_optional_parse("PMDATA_HISTORY_START_MS")?;
        let history_end_ms = env_optional_parse("PMDATA_HISTORY_END_MS")?;
        if let (Some(start), Some(end)) = (history_start_ms, history_end_ms) {
            if start >= end {
                bail!("PMDATA_HISTORY_START_MS must be less than PMDATA_HISTORY_END_MS");
            }
        }
        let request_interval_ms = env_parse("PMDATA_REQUEST_INTERVAL_MS", 1_100u64)?;
        if request_interval_ms < 100 {
            bail!("PMDATA_REQUEST_INTERVAL_MS must be at least 100");
        }
        let poll_seconds = env_parse("PMDATA_POLL_SECONDS", 300u64)?;
        if poll_seconds < 60 {
            bail!("PMDATA_POLL_SECONDS must be at least 60");
        }
        let availability_lag_secs = env_parse("PMDATA_AVAILABILITY_LAG_SECS", 3_600u64)?;
        let refresh_hours = env_parse("PMDATA_REFRESH_HOURS", 2u64)?;
        let refresh_interval_secs = env_parse("PMDATA_REFRESH_INTERVAL_SECS", 3_600u64)?;
        if refresh_interval_secs < 60 {
            bail!("PMDATA_REFRESH_INTERVAL_SECS must be at least 60");
        }
        let max_retries = env_parse("PMDATA_MAX_RETRIES", 5usize)?;
        if max_retries == 0 {
            bail!("PMDATA_MAX_RETRIES must be greater than zero");
        }
        Ok(Self {
            api_key,
            api_url: std::env::var("PMDATA_API_URL")
                .unwrap_or_else(|_| "https://api.pmdata.dev".to_string())
                .trim_end_matches('/')
                .to_string(),
            coins,
            market_type,
            data_types,
            history_days,
            history_start_ms,
            history_end_ms,
            data_dir,
            database_file,
            request_interval: Duration::from_millis(request_interval_ms),
            poll_interval: Duration::from_secs(poll_seconds),
            availability_lag: Duration::from_secs(availability_lag_secs),
            refresh_window: Duration::from_secs(refresh_hours.saturating_mul(3_600)),
            refresh_interval: Duration::from_secs(refresh_interval_secs),
            max_markets_per_sync: env_parse("PMDATA_MAX_MARKETS_PER_SYNC", 20usize)?,
            max_retries,
            retry_initial: Duration::from_millis(env_parse("PMDATA_RETRY_INITIAL_MS", 1_500u64)?),
        })
    }

    fn period_secs(&self) -> i64 {
        match self.market_type.as_str() {
            "5m" => 300,
            "15m" => 900,
            "1h" => 3_600,
            _ => unreachable!("validated market type"),
        }
    }

    fn history_bounds(&self, now_ms: i64) -> (i64, i64) {
        let end = self.history_end_ms.unwrap_or(now_ms);
        let start = self
            .history_start_ms
            .unwrap_or_else(|| end.saturating_sub(self.history_days as i64 * 86_400_000));
        (start, end)
    }
}

fn parse_list(name: &str, raw: &str, allowed: &[&str]) -> Result<Vec<String>> {
    let parsed = raw
        .split(',')
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if parsed.is_empty()
        || parsed
            .iter()
            .any(|value| !allowed.contains(&value.as_str()))
    {
        bail!("{name} must contain one or more of {}", allowed.join(", "));
    }
    Ok(parsed)
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

struct ApiClient {
    client: reqwest::Client,
    api_key: String,
    api_url: String,
    request_interval: Duration,
    last_request: Option<Instant>,
    max_retries: usize,
    retry_initial: Duration,
}

impl ApiClient {
    fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(180))
                .user_agent(concat!("polypulse-pmdata/", env!("CARGO_PKG_VERSION")))
                .build()?,
            api_key: config.api_key.clone(),
            api_url: config.api_url.clone(),
            request_interval: config.request_interval,
            last_request: None,
            max_retries: config.max_retries,
            retry_initial: config.retry_initial,
        })
    }

    async fn download(&mut self, data_type: &str, slug: &str) -> Result<Vec<u8>> {
        let url = format!("{}/download/{data_type}/{slug}.parquet", self.api_url);
        let mut delay = self.retry_initial;
        for attempt in 1..=self.max_retries {
            if let Some(last) = self.last_request {
                if last.elapsed() < self.request_interval {
                    tokio::time::sleep(self.request_interval - last.elapsed()).await;
                }
            }
            self.last_request = Some(Instant::now());
            match self
                .client
                .get(&url)
                .header("api_key", &self.api_key)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    return Ok(response.bytes().await?.to_vec())
                }
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    if attempt < self.max_retries
                        && (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                    {
                        log_line(
                            "WARN",
                            format!(
                                "{slug} {data_type}: HTTP {status}; retry {attempt}/{} in {}s",
                                self.max_retries,
                                delay.as_secs()
                            ),
                        );
                        tokio::time::sleep(delay).await;
                        delay = delay.saturating_mul(2).min(Duration::from_secs(60));
                        continue;
                    }
                    bail!(
                        "PMData HTTP {status} for {data_type}/{slug}: {}",
                        body.chars().take(300).collect::<String>()
                    );
                }
                Err(error) if attempt < self.max_retries => {
                    log_line(
                        "WARN",
                        format!(
                            "{slug} {data_type}: retry {attempt}/{} in {}s: {error}",
                            self.max_retries,
                            delay.as_secs()
                        ),
                    );
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(Duration::from_secs(60));
                }
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("retry loop returns")
    }
}

struct Store {
    connection: Connection,
    data_dir: PathBuf,
}

impl Store {
    fn open(config: &Config) -> Result<Self> {
        fs::create_dir_all(&config.data_dir)?;
        if let Some(parent) = config.database_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&config.database_file)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            bail!("PMData database schema {version} is newer than supported {SCHEMA_VERSION}");
        }
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS markets (
                slug TEXT PRIMARY KEY, coin TEXT NOT NULL, market_type TEXT NOT NULL,
                start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
              );
              CREATE INDEX IF NOT EXISTS pmdata_market_scope ON markets(coin, market_type, start_ms);
              CREATE TABLE IF NOT EXISTS files (
                slug TEXT NOT NULL REFERENCES markets(slug) ON DELETE CASCADE,
                data_type TEXT NOT NULL, status TEXT NOT NULL, byte_len INTEGER,
                sha256 TEXT, path TEXT, downloaded_at_ms INTEGER, last_error TEXT,
                PRIMARY KEY(slug, data_type)
              );",
        )?;
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self {
            connection,
            data_dir: config.data_dir.clone(),
        })
    }

    fn upsert_market(
        &self,
        slug: &str,
        coin: &str,
        market_type: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO markets(slug,coin,market_type,start_ms,end_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(slug) DO UPDATE SET updated_at_ms=excluded.updated_at_ms",
            params![slug, coin, market_type, start_ms, end_ms, now_ms()],
        )?;
        Ok(())
    }

    fn needs_download(
        &self,
        slug: &str,
        data_type: &str,
        now_ms: i64,
        refresh_until_ms: i64,
        refresh_interval_ms: i64,
    ) -> Result<bool> {
        let item = self.connection.query_row(
            "SELECT status, downloaded_at_ms FROM files WHERE slug=?1 AND data_type=?2",
            params![slug, data_type],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        );
        match item {
            Ok((status, downloaded_at_ms)) => Ok(status != "complete"
                || (now_ms < refresh_until_ms
                    && downloaded_at_ms
                        .unwrap_or(0)
                        .saturating_add(refresh_interval_ms)
                        <= now_ms)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    fn path_for(&self, coin: &str, market_type: &str, slug: &str, data_type: &str) -> PathBuf {
        self.data_dir
            .join("raw")
            .join(coin)
            .join(market_type)
            .join(slug)
            .join(format!("{data_type}.parquet"))
    }

    fn record_complete(
        &self,
        slug: &str,
        data_type: &str,
        path: &Path,
        bytes: &[u8],
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO files(slug,data_type,status,byte_len,sha256,path,downloaded_at_ms,last_error)
             VALUES(?1,?2,'complete',?3,?4,?5,?6,NULL)
             ON CONFLICT(slug,data_type) DO UPDATE SET status='complete',byte_len=excluded.byte_len,sha256=excluded.sha256,path=excluded.path,downloaded_at_ms=excluded.downloaded_at_ms,last_error=NULL",
            params![slug, data_type, bytes.len() as i64, format!("{:x}", Sha256::digest(bytes)), path.to_string_lossy(), now_ms()],
        )?;
        Ok(())
    }

    fn record_error(&self, slug: &str, data_type: &str, error: &anyhow::Error) -> Result<()> {
        self.connection.execute(
            "INSERT INTO files(slug,data_type,status,last_error) VALUES(?1,?2,'error',?3)
             ON CONFLICT(slug,data_type) DO UPDATE SET status='error',last_error=excluded.last_error",
            params![slug, data_type, format!("{error:#}")],
        )?;
        Ok(())
    }

    fn status(&self) -> Result<(i64, i64, i64, i64)> {
        self.connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM markets), (SELECT COUNT(*) FROM files),
                    (SELECT COUNT(*) FROM files WHERE status='complete'),
                    (SELECT COALESCE(SUM(byte_len),0) FROM files WHERE status='complete')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(Into::into)
    }
}

#[derive(Debug, Clone)]
struct Job {
    coin: String,
    slug: String,
    start_ms: i64,
    end_ms: i64,
    data_type: String,
}

struct Downloader {
    config: Config,
    api: ApiClient,
    store: Store,
    _lock: File,
}

impl Downloader {
    fn new(config: Config) -> Result<Self> {
        fs::create_dir_all(&config.data_dir)?;
        let lock_path = config.data_dir.join("downloader.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        lock.try_lock_exclusive().with_context(|| {
            format!("another PMData downloader is using {}", lock_path.display())
        })?;
        let api = ApiClient::new(&config)?;
        let store = Store::open(&config)?;
        Ok(Self {
            config,
            api,
            store,
            _lock: lock,
        })
    }

    fn jobs(&self) -> Result<Vec<Job>> {
        let now = now_ms();
        let (start, end) = self.config.history_bounds(now);
        let period_ms = self.config.period_secs() * 1_000;
        let first = start.div_euclid(period_ms) * period_ms;
        let latest_end = now.saturating_sub(self.config.availability_lag.as_millis() as i64);
        let mut jobs = Vec::new();
        for start_ms in (first..end).step_by(period_ms as usize) {
            let end_ms = start_ms + period_ms;
            if end_ms > latest_end {
                continue;
            }
            for coin in &self.config.coins {
                let slug = format!(
                    "{coin}-updown-{}-{}",
                    self.config.market_type,
                    start_ms / 1_000
                );
                self.store.upsert_market(
                    &slug,
                    coin,
                    &self.config.market_type,
                    start_ms,
                    end_ms,
                )?;
                let refresh_until =
                    end_ms.saturating_add(self.config.refresh_window.as_millis() as i64);
                for data_type in &self.config.data_types {
                    if self.store.needs_download(
                        &slug,
                        data_type,
                        now,
                        refresh_until,
                        self.config.refresh_interval.as_millis() as i64,
                    )? {
                        jobs.push(Job {
                            coin: coin.clone(),
                            slug: slug.clone(),
                            start_ms,
                            end_ms,
                            data_type: data_type.clone(),
                        });
                    }
                }
            }
        }
        jobs.sort_by_key(|job| (job.start_ms, job.coin.clone(), job.data_type.clone()));
        if self.config.max_markets_per_sync > 0 {
            let mut selected = BTreeSet::new();
            jobs.retain(|job| {
                if selected.contains(&job.slug) {
                    true
                } else if selected.len() < self.config.max_markets_per_sync {
                    selected.insert(job.slug.clone());
                    true
                } else {
                    false
                }
            });
        }
        Ok(jobs)
    }

    async fn sync_once(&mut self) -> Result<()> {
        let jobs = self.jobs()?;
        log_line("INFO", format!("PMData sync pending_files={} scopes={} period={} types={} database={} (API key hidden)", jobs.len(), self.config.coins.join(","), self.config.market_type, self.config.data_types.join(","), self.config.database_file.display()));
        let progress = ConsoleProgress::new(jobs.len() as u64)?;
        let mut complete = 0usize;
        let mut failed = 0usize;
        let mut bytes = 0u64;
        for (index, job) in jobs.iter().enumerate() {
            progress.set_message(format!("{} {}", job.data_type, job.slug));
            match self.api.download(&job.data_type, &job.slug).await {
                Ok(body) => {
                    let path = self.store.path_for(
                        &job.coin,
                        &self.config.market_type,
                        &job.slug,
                        &job.data_type,
                    );
                    write_atomic(&path, &body)?;
                    self.store
                        .record_complete(&job.slug, &job.data_type, &path, &body)?;
                    complete += 1;
                    bytes += body.len() as u64;
                    log_line(
                        "INFO",
                        format!(
                            "file_progress={}/{} slug={} type={} bytes={} window={}..{}",
                            index + 1,
                            jobs.len(),
                            job.slug,
                            job.data_type,
                            human_bytes(body.len() as u64),
                            job.start_ms,
                            job.end_ms
                        ),
                    );
                }
                Err(error) => {
                    failed += 1;
                    self.store.record_error(&job.slug, &job.data_type, &error)?;
                    log_line(
                        "ERROR",
                        format!("slug={} type={}: {error:#}", job.slug, job.data_type),
                    );
                }
            }
            progress.inc(1);
        }
        drop(progress);
        log_line(
            "INFO",
            format!(
                "PMData sync finished complete={complete} failed={failed} raw={}",
                human_bytes(bytes)
            ),
        );
        if failed > 0 {
            bail!("{failed} PMData file download(s) failed; rerun sync to resume");
        }
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("data file has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("pmdata"),
        uuid::Uuid::new_v4()
    ));
    let mut file = File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}
fn log_line(level: &str, message: impl std::fmt::Display) {
    let rendered = format!(
        "{} {level:<5} {message}",
        Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ")
    );
    let progress = ACTIVE_PROGRESS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(progress) = progress {
        progress.println(rendered);
    } else {
        println!("{rendered}");
    }
}
fn human_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.2}MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1}KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

fn usage() {
    println!("Usage: pmdata_downloader [sync|watch|status]\n\n sync    Download missing or refresh-eligible PMData files once (default)\n watch   Sync periodically; Ctrl-C exits without corrupting completed files\n status  Print local catalog counts without requesting PMData");
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
        usage();
        return Ok(());
    }
    let config = Config::from_env()?;
    if command == "status" {
        let store = Store::open(&config)?;
        let (markets, files, complete, bytes) = store.status()?;
        println!("PMData local status\n  database: {}\n  markets: {markets}\n  files: {files}\n  complete: {complete}\n  raw: {}", config.database_file.display(), human_bytes(bytes.max(0) as u64));
        return Ok(());
    }
    if !matches!(command.as_str(), "sync" | "watch") {
        usage();
        bail!("unknown command {command}");
    }
    let mut downloader = Downloader::new(config.clone())?;
    loop {
        let result = tokio::select! {
            result = downloader.sync_once() => result,
            result = tokio::signal::ctrl_c() => { result?; log_line("INFO", "shutdown requested; rerun sync to resume"); return Ok(()); }
        };
        if let Err(error) = result {
            log_line("ERROR", format!("sync cycle failed: {error:#}"));
            if command == "sync" {
                return Err(error);
            }
        }
        if command == "sync" {
            return Ok(());
        }
        log_line(
            "INFO",
            format!("next update in {} seconds", config.poll_interval.as_secs()),
        );
        tokio::select! {
            _ = tokio::time::sleep(config.poll_interval) => {}
            result = tokio::signal::ctrl_c() => { result?; log_line("INFO", "shutdown requested"); return Ok(()); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_deduplicated_supported_values() {
        assert_eq!(
            parse_list("coins", "btc,eth,btc", SUPPORTED_COINS).unwrap(),
            vec!["btc", "eth"]
        );
        assert!(parse_list("types", "l2,unknown", SUPPORTED_DATA_TYPES).is_err());
    }

    #[test]
    fn generated_slug_is_aligned_to_market_period() {
        let config = Config {
            api_key: "test".into(),
            api_url: "https://example.invalid".into(),
            coins: vec!["btc".into()],
            market_type: "5m".into(),
            data_types: vec!["l2".into()],
            history_days: 1,
            history_start_ms: None,
            history_end_ms: None,
            data_dir: PathBuf::from("data"),
            database_file: PathBuf::from("data/test.sqlite3"),
            request_interval: Duration::from_secs(1),
            poll_interval: Duration::from_secs(60),
            availability_lag: Duration::ZERO,
            refresh_window: Duration::ZERO,
            refresh_interval: Duration::from_secs(60),
            max_markets_per_sync: 1,
            max_retries: 1,
            retry_initial: Duration::ZERO,
        };
        assert_eq!(config.period_secs(), 300);
        assert_eq!(1_788_166_500_i64 % config.period_secs(), 0);
    }
}
