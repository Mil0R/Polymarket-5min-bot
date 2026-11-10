//! On-demand Unix socket snapshots: no market requests, no account operations.
use crate::{late_resolution_winner_model::ChainlinkTwapStore, runtime_metrics::RuntimeMetrics};
use anyhow::{Context, Result};
use chrono::Utc;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Quote {
    pub asset: String,
    pub slug: String,
    pub window_start: i64,
    pub twap: Option<Decimal>,
    pub open: Option<Decimal>,
    pub reference_ms: Option<i64>,
    pub up_ask: Option<Decimal>,
    pub down_ask: Option<Decimal>,
    pub up_ms: Option<i64>,
    pub down_ms: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    #[serde(default)]
    pub operator: Option<super::control::Status>,
    pub pid: u32,
    pub run_id: String,
    pub journal: String,
    pub started_ms: i64,
    pub captured_ms: i64,
    pub loop_ms: i64,
    pub scan_ms: Option<i64>,
    pub settlement_ms: Option<i64>,
    pub target_window: i64,
    pub phase: String,
    pub scans: u64,
    pub market_ws_connected: bool,
    pub feed_max_age_ms: i64,
    pub book_max_age_ms: i64,
    pub rest_calls: u64,
    pub rest_successes: u64,
    pub rest_total_ms: u64,
    pub ws_reconnects: u64,
    pub quotes: Vec<Quote>,
}
pub(crate) struct LiveState {
    start: Instant,
    started_ms: i64,
    run_id: String,
    journal: String,
    loop_elapsed: AtomicU64,
    scan_elapsed: AtomicU64,
    settlement_elapsed: AtomicU64,
    target: AtomicI64,
    phase: AtomicU8,
    viewer_ms: AtomicI64,
    connected: AtomicBool,
    books: Mutex<[Quote; 2]>,
    operator: Mutex<Option<Arc<crate::late_resolution_winner_price_cap::PriceCapStudy>>>,
}
impl LiveState {
    pub(crate) fn new(run_id: &str, journal: &Path) -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            started_ms: Utc::now().timestamp_millis(),
            run_id: run_id.into(),
            journal: canonical(journal).to_string_lossy().into_owned(),
            loop_elapsed: AtomicU64::new(0),
            scan_elapsed: AtomicU64::new(0),
            settlement_elapsed: AtomicU64::new(0),
            target: AtomicI64::new(0),
            phase: AtomicU8::new(0),
            viewer_ms: AtomicI64::new(0),
            connected: AtomicBool::new(false),
            operator: Mutex::new(None),
            books: Mutex::new([
                Quote {
                    asset: "btc".into(),
                    ..Default::default()
                },
                Quote {
                    asset: "eth".into(),
                    ..Default::default()
                },
            ]),
        })
    }
    pub(crate) fn set_operator(
        &self,
        study: Arc<crate::late_resolution_winner_price_cap::PriceCapStudy>,
    ) {
        *self.operator.lock().unwrap() = Some(study);
    }
    fn elapsed(&self) -> u64 {
        self.start.elapsed().as_millis() as u64 + 1
    }
    pub(crate) fn loop_tick(&self) {
        self.loop_elapsed.store(self.elapsed(), Ordering::Relaxed);
    }
    pub(crate) fn scanned(&self) {
        self.scan_elapsed.store(self.elapsed(), Ordering::Relaxed);
    }
    pub(crate) fn settling(&self) {
        self.settlement_elapsed
            .store(self.elapsed(), Ordering::Relaxed);
    }
    pub(crate) fn discovering(&self, target: i64) {
        self.target.store(target, Ordering::Relaxed);
        self.phase.store(1, Ordering::Relaxed);
    }
    pub(crate) fn monitoring(&self) {
        self.phase.store(2, Ordering::Relaxed);
    }
    pub(crate) fn connected(&self, value: bool) {
        self.connected.store(value, Ordering::Relaxed);
    }
    pub(crate) fn interested(&self) -> bool {
        Utc::now()
            .timestamp_millis()
            .saturating_sub(self.viewer_ms.load(Ordering::Relaxed))
            < 3000
    }
    pub(crate) fn book(&self, quote: Quote) {
        if let Ok(mut rows) = self.books.try_lock() {
            let i = if quote.asset.eq_ignore_ascii_case("btc") {
                0
            } else if quote.asset.eq_ignore_ascii_case("eth") {
                1
            } else {
                return;
            };
            rows[i] = quote;
        }
    }
    fn at(&self, elapsed: &AtomicU64) -> Option<i64> {
        let x = elapsed.load(Ordering::Relaxed);
        (x > 0).then_some(self.started_ms + x as i64 - 1)
    }
    fn snapshot(
        &self,
        store: &ChainlinkTwapStore,
        metrics: &RuntimeMetrics,
        feed: i64,
        book: i64,
        skew: i64,
    ) -> Snapshot {
        let now = Utc::now().timestamp_millis();
        let window = now / 300_000 * 300;
        let mut quotes = self.books.try_lock().map(|x| x.clone()).unwrap_or_default();
        for (i, asset) in ["btc", "eth"].iter().enumerate() {
            if quotes[i].window_start != window {
                quotes[i] = Quote {
                    asset: (*asset).into(),
                    window_start: window,
                    ..Default::default()
                };
            }
            let (current, open) = store.dashboard_samples(asset, window * 1000, skew);
            quotes[i].asset = (*asset).into();
            quotes[i].twap = current.map(|s| s.value);
            quotes[i].reference_ms = current.map(|s| s.observed_at_ms);
            quotes[i].open = open.map(|s| s.value);
        }
        let m = metrics.snapshot();
        Snapshot {
            version: 1,
            operator: self
                .operator
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|s| s.operator_status()),
            pid: std::process::id(),
            run_id: self.run_id.clone(),
            journal: self.journal.clone(),
            started_ms: self.started_ms,
            captured_ms: now,
            loop_ms: self.at(&self.loop_elapsed).unwrap_or(self.started_ms),
            scan_ms: self.at(&self.scan_elapsed),
            settlement_ms: self.at(&self.settlement_elapsed),
            target_window: self.target.load(Ordering::Relaxed),
            phase: match self.phase.load(Ordering::Relaxed) {
                1 => "发现市场",
                2 => "监控窗口",
                _ => "初始化",
            }
            .into(),
            scans: m.late_resolution_winner_trading_scans,
            market_ws_connected: self.connected.load(Ordering::Relaxed),
            feed_max_age_ms: feed,
            book_max_age_ms: book,
            rest_calls: m.late_resolution_winner_trading_rest_fallbacks,
            rest_successes: m.late_resolution_winner_trading_rest_successes,
            rest_total_ms: m.late_resolution_winner_trading_rest_latency_ms_total,
            ws_reconnects: m.sdk_market_ws_reconnects,
            quotes: quotes.into(),
        }
    }
}
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        let mut normalized = PathBuf::new();
        for part in full.components() {
            match part {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                _ => normalized.push(part),
            }
        }
        normalized
    })
}
pub fn socket_path(journal: &Path) -> PathBuf {
    let digest = Sha256::digest(canonical(journal).to_string_lossy().as_bytes());
    let hash = format!("{digest:x}");
    #[cfg(unix)]
    let uid = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self")
            .map(|m| m.uid())
            .unwrap_or(0)
    };
    #[cfg(not(unix))]
    let uid = 0;
    std::env::temp_dir()
        .join(format!("lrw-tui-{uid}"))
        .join(format!("{}.sock", &hash[..20]))
}
#[cfg(unix)]
pub(crate) fn start(
    state: Arc<LiveState>,
    store: Arc<ChainlinkTwapStore>,
    metrics: Arc<RuntimeMetrics>,
    shutdown: Arc<AtomicBool>,
    feed: i64,
    book: i64,
    skew: i64,
) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
    use tokio::{io::AsyncWriteExt, net::UnixListener};
    let path = socket_path(Path::new(&state.journal));
    let dir = path.parent().context("socket目录不存在")?;
    if !dir.exists() {
        std::fs::DirBuilder::new().mode(0o700).create(dir)?;
    }
    let meta = std::fs::symlink_metadata(dir)?;
    let uid = std::fs::metadata("/proc/self")?.uid();
    anyhow::ensure!(
        meta.is_dir()
            && !meta.file_type().is_symlink()
            && meta.uid() == uid
            && meta.mode() & 0o077 == 0,
        "socket目录必须由当前用户独占"
    );
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        anyhow::ensure!(
            meta.file_type().is_socket() && meta.uid() == uid,
            "拒绝覆盖非自有socket"
        );
        anyhow::ensure!(
            std::os::unix::net::UnixStream::connect(&path).is_err(),
            "已有活动面板socket"
        );
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    if let Some(study) = state.operator.lock().unwrap().clone() {
        start_commands(&path, study, shutdown.clone())?;
    }
    let inode = std::fs::metadata(&path)?.ino();
    let cleanup = path.clone();
    tokio::spawn(async move {
        use std::os::unix::fs::MetadataExt;
        struct Cleanup(PathBuf, u64);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if std::fs::metadata(&self.0).is_ok_and(|m| m.ino() == self.1) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
        }
        let _cleanup = Cleanup(cleanup, inode);
        let mut cached = Vec::new();
        let mut last = Instant::now() - Duration::from_secs(2);
        let slots = Arc::new(tokio::sync::Semaphore::new(4));
        loop {
            tokio::select! {
                result=listener.accept()=>{
                    let Ok((mut stream,_))=result else{break;};let Ok(permit)=slots.clone().try_acquire_owned() else{continue;};
                    state.viewer_ms.store(Utc::now().timestamp_millis(),Ordering::Relaxed);
                    if last.elapsed()>=Duration::from_secs(1){
                        match serde_json::to_vec(&state.snapshot(&store,&metrics,feed,book,skew)){Ok(mut bytes)=>{bytes.push(b'\n');cached=bytes;last=Instant::now();},Err(_)=>continue}
                    }
                    let bytes=cached.clone();tokio::spawn(async move{let _permit=permit;let _=tokio::time::timeout(Duration::from_millis(200),stream.write_all(&bytes)).await;});
                }
                _=tokio::time::sleep(Duration::from_millis(500))=>{if shutdown.load(Ordering::Relaxed){break;}}
            }
        }
    });
    Ok(path)
}
#[cfg(not(unix))]
pub(crate) fn start(
    _: Arc<LiveState>,
    _: Arc<ChainlinkTwapStore>,
    _: Arc<RuntimeMetrics>,
    _: Arc<AtomicBool>,
    _: i64,
    _: i64,
    _: i64,
) -> Result<PathBuf> {
    anyhow::bail!("实时TUI需要Unix socket");
}

pub fn read(path: &Path, journal: &Path) -> Result<Snapshot> {
    #[cfg(unix)]
    {
        use std::io::{BufRead, BufReader, Read};
        use std::os::unix::net::UnixStream;
        let stream = UnixStream::connect(path).context("未连接策略快照服务")?;
        stream.set_read_timeout(Some(Duration::from_millis(350)))?;
        let mut line = String::new();
        BufReader::new(stream.take(64 * 1024)).read_line(&mut line)?;
        anyhow::ensure!(line.ends_with('\n'), "快照不完整");
        let snap: Snapshot = serde_json::from_str(&line)?;
        anyhow::ensure!(
            snap.version == 1 && canonical(Path::new(&snap.journal)) == canonical(journal),
            "快照批次与账本不匹配"
        );
        Ok(snap)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, journal);
        anyhow::bail!("实时TUI需要Unix socket")
    }
}

#[cfg(unix)]
fn start_commands(
    snapshot_path: &Path,
    study: Arc<crate::late_resolution_winner_price_cap::PriceCapStudy>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let path = super::control::socket(snapshot_path);
    let uid = std::fs::metadata("/proc/self")?.uid();
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        anyhow::ensure!(
            meta.file_type().is_socket() && meta.uid() == uid,
            "unsafe control socket"
        );
        anyhow::ensure!(
            std::os::unix::net::UnixStream::connect(&path).is_err(),
            "control socket already active"
        );
        std::fs::remove_file(&path)?;
    }
    let listener = tokio::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let inode = std::fs::metadata(&path)?.ino();
    tokio::spawn(async move {
        struct Cleanup(std::path::PathBuf, u64);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if std::fs::metadata(&self.0).is_ok_and(|m| m.ino() == self.1) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
        }
        let _cleanup = Cleanup(path, inode);
        let slots = Arc::new(tokio::sync::Semaphore::new(2));
        loop {
            tokio::select! {
                result = listener.accept() => {
                    let Ok((stream, _)) = result else { break; };
                    if !stream.peer_cred().is_ok_and(|c| c.uid() == uid) { continue; }
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                    let study = study.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let mut reader = BufReader::new(stream.take(8193));
                        let mut bytes = Vec::new();
                        if !matches!(tokio::time::timeout(Duration::from_millis(500), reader.read_until(b'\n', &mut bytes)).await, Ok(Ok(_)))
                            || bytes.len() > 8192 || bytes.last() != Some(&b'\n') { return; }
                        let Ok(command) = serde_json::from_slice::<super::control::Command>(&bytes) else { return; };
                        let Ok(reply) = tokio::task::spawn_blocking(move || study.command(command)).await else { return; };
                        let Ok(mut bytes) = serde_json::to_vec(&reply) else { return; };
                        bytes.push(b'\n');
                        let mut stream = reader.into_inner().into_inner();
                        let _ = tokio::time::timeout(Duration::from_millis(500), stream.write_all(&bytes)).await;
                    });
                }
                _ = tokio::time::sleep(Duration::from_millis(200)) => { if shutdown.load(Ordering::Relaxed) { break; } }
            }
        }
    });
    Ok(())
}
