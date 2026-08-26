//! Copy-trading logging & results journal.
//!
//! Two layers, mirroring the main bot's conventions:
//!
//! 1. **File logging** — `init_copy_trader_logging` writes tracing output to
//!    BOTH stdout and `LOG_FILE` (default `logs/copy-trader.log`) with size
//!    rotation (`LOG_ROTATE_MAX_BYTES` / `LOG_ROTATE_FILES`), so long runs can
//!    be reviewed after the fact.
//!
//! 2. **Structured results** — `CopyTradeJournal` appends one JSON object per
//!    line (NDJSON) to `results/copy-trading/{mode}/events.ndjson`, capturing
//!    signals, consensus, proposals, orders, settlements, ladder exits, wallet
//!    enrollment and periodic NAV snapshots. Together with the run metadata
//!    line this enables post-run regression analysis (e.g. replaying signals
//!    against the same config, or auditing execution quality).

use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::info;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

// ---------------------------------------------------------------------------
// Size-rotating file writer (same semantics as the main bot's logger)
// ---------------------------------------------------------------------------

pub struct RotatingFileWriter {
    path: PathBuf,
    file: Option<File>,
    bytes_written: u64,
    max_bytes: u64,
    retention: usize,
}

impl RotatingFileWriter {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let max_bytes = std::env::var("LOG_ROTATE_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10 * 1024 * 1024);
        let retention = std::env::var("LOG_ROTATE_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        Self::new_with_limits(path, max_bytes, retention)
    }

    /// Explicit limits (env-independent; used by tests to avoid cross-test
    /// env-var races when tests run in parallel).
    pub fn new_with_limits(
        path: impl AsRef<Path>,
        max_bytes: u64,
        retention: usize,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        rotate_log_file(&path, max_bytes, retention)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            path,
            file: Some(file),
            bytes_written,
            max_bytes: max_bytes.max(1),
            retention,
        })
    }

    fn rotate_before(&mut self, incoming: usize) -> io::Result<()> {
        if self.retention == 0
            || self.bytes_written == 0
            || self.bytes_written.saturating_add(incoming as u64) <= self.max_bytes
        {
            return Ok(());
        }
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            drop(file);
        }
        rotate_log_file(&self.path, 1, self.retention)
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
        );
        self.bytes_written = 0;
        Ok(())
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.rotate_before(buffer.len())?;
        let written = self
            .file
            .as_mut()
            .expect("rotating writer must keep a file open")
            .write(buffer)?;
        self.bytes_written = self.bytes_written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .expect("rotating writer must keep a file open")
            .flush()
    }
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), index))
}

fn rotate_log_file(path: &Path, max_bytes: u64, retention: usize) -> Result<()> {
    if retention == 0 || !path.exists() || fs::metadata(path)?.len() < max_bytes.max(1) {
        return Ok(());
    }
    let oldest = rotated_path(path, retention);
    if oldest.exists() {
        fs::remove_file(oldest)?;
    }
    for index in (1..retention).rev() {
        let source = rotated_path(path, index);
        if source.exists() {
            fs::rename(source, rotated_path(path, index + 1))?;
        }
    }
    fs::rename(path, rotated_path(path, 1))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Logging init (stdout + rotating file)
// ---------------------------------------------------------------------------

/// Initializes tracing with BOTH stdout (interactive) and a rotating file
/// (`LOG_FILE`, defaulting to `default_log_file`). Safe to call once per process.
pub fn init_copy_trader_logging(default_log_file: &str) -> Result<()> {
    dotenvy::dotenv().ok();
    let mut env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,polypulse=debug"));

    // The public APIs add response fields ahead of SDK releases. The SDK
    // accepts those fields but emits one WARN per field, which can turn a
    // healthy startup into thousands of misleading lines. Keep actual SDK
    // errors visible and suppress only this compatibility target, including
    // when a shell-level `RUST_LOG=info` overrides the .env filter.
    let show_sdk_schema_warnings = std::env::var("COPY_SHOW_SDK_SCHEMA_WARNINGS")
        .map(|v| v.trim().eq_ignore_ascii_case("true") || v.trim() == "1")
        .unwrap_or(false);
    if !show_sdk_schema_warnings {
        env_filter = env_filter.add_directive(
            "polymarket_client_sdk::serde_helpers=error"
                .parse()
                .map_err(anyhow::Error::msg)?,
        );
    }

    let log_path = std::env::var("COPY_TRADING_LOG_FILE")
        .or_else(|_| std::env::var("LOG_FILE"))
        .unwrap_or_else(|_| default_log_file.to_string());
    let file_writer = RotatingFileWriter::new(&log_path)?;

    let stdout_layer = tracing_subscriber::fmt::layer().with_ansi(true);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(BoxMakeWriter::new(Mutex::new(file_writer)))
        .with_ansi(false);

    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_layer)
        .with(env_filter)
        .init();
    info!(log_file = %log_path, "File logging enabled");
    if !show_sdk_schema_warnings {
        info!("SDK schema compatibility warnings suppressed; set COPY_SHOW_SDK_SCHEMA_WARNINGS=true to diagnose API field changes");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Structured results journal (NDJSON)
// ---------------------------------------------------------------------------

/// Appends structured events to `results/copy-trading/{mode}/events.ndjson`.
/// Thread-safe; every line is a complete JSON object for easy replay/analysis.
#[derive(Clone)]
pub struct CopyTradeJournal {
    run_id: String,
    events: Arc<Mutex<Option<RotatingFileWriter>>>,
}

use std::sync::Arc;

impl CopyTradeJournal {
    pub fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            events: Arc::new(Mutex::new(None)),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Opens (and creates parent dirs for) the NDJSON events file.
    pub fn enable(&self, path: impl AsRef<Path>) -> Result<()> {
        let writer = RotatingFileWriter::new(path)?;
        *self.events.lock().expect("journal lock poisoned") = Some(writer);
        Ok(())
    }

    /// Whether a results file is currently open.
    pub fn is_enabled(&self) -> bool {
        self.events.lock().expect("journal lock poisoned").is_some()
    }

    /// Writes one event line: `{"ts":..., "runId":..., "type":..., ...fields}`.
    pub fn record(&self, event_type: &str, fields: serde_json::Value) {
        let mut entry = json!({
            "ts": Utc::now().to_rfc3339(),
            "runId": self.run_id,
            "type": event_type,
        });
        if let serde_json::Value::Object(map) = &mut entry {
            if let serde_json::Value::Object(f) = fields {
                map.extend(f);
            }
        }
        let line = match serde_json::to_string(&entry) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "journal serialization failed");
                return;
            }
        };
        let mut guard = self.events.lock().expect("journal lock poisoned");
        if let Some(writer) = guard.as_mut() {
            if let Err(e) = writeln!(writer, "{}", line).and_then(|_| writer.flush()) {
                tracing::warn!(error = %e, "journal write failed");
            }
        }
    }

    /// Convenience: run metadata line written once at startup.
    pub fn record_run_meta(&self, mode: &str, fields: serde_json::Value) {
        let mut meta = json!({
            "mode": mode,
        });
        if let serde_json::Value::Object(m) = &mut meta {
            if let serde_json::Value::Object(f) = fields {
                m.extend(f);
            }
        }
        self.record("run_meta", meta);
    }

    /// Periodic NAV snapshot for equity-curve regression analysis.
    pub fn record_nav_snapshot(
        &self,
        nav: rust_decimal::Decimal,
        cash: rust_decimal::Decimal,
        open_positions: usize,
        whitelist_wallets: usize,
    ) {
        self.record(
            "nav_snapshot",
            json!({
                "nav": nav.to_string(),
                "cash": cash.to_string(),
                "openPositions": open_positions,
                "whitelistWallets": whitelist_wallets,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    #[test]
    fn journal_appends_json_lines() {
        let dir = std::env::temp_dir().join(format!("copy-journal-{}", std::process::id()));
        let path = dir.join("events.ndjson");
        let _ = std::fs::remove_dir_all(&dir);

        let journal = CopyTradeJournal::new("test-run");
        journal.enable(&path).unwrap();
        assert!(journal.is_enabled());

        journal.record("signal", json!({"wallet": "0xabc", "action": "OpenBuy"}));
        journal.record("order", json!({"orderId": "o1", "filled": "100"}));
        journal.record_nav_snapshot(
            rust_decimal_macros::dec!(9999.5),
            rust_decimal_macros::dec!(9700),
            2,
            3,
        );

        let file = std::fs::File::open(&path).unwrap();
        let lines: Vec<String> = std::io::BufReader::new(file)
            .lines()
            .map(|l| l.unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON per line");
            assert_eq!(v["runId"], "test-run");
            assert!(v["ts"].is_string());
        }
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&lines[0]).unwrap()["type"],
            "signal"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&lines[1]).unwrap()["orderId"],
            "o1"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&lines[2]).unwrap()["type"],
            "nav_snapshot"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_disabled_is_noop() {
        let journal = CopyTradeJournal::new("noop");
        journal.record("signal", json!({})); // must not panic
        assert!(!journal.is_enabled());
    }

    #[test]
    fn rotating_writer_rotates_by_size() {
        let dir = std::env::temp_dir().join(format!("copy-rotate-{}", std::process::id()));
        let path = dir.join("test.log");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut writer = RotatingFileWriter::new_with_limits(&path, 16, 2).unwrap();
        writer.write_all(b"0123456789").unwrap(); // 10 bytes
        writer.write_all(b"0123456789").unwrap(); // 10+10=20 > 16 -> rotate
        writer.write_all(b"tail").unwrap();
        writer.flush().unwrap();
        drop(writer);

        assert!(rotated_path(&path, 1).exists(), "rotated file must exist");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0123456789tail");
        assert_eq!(
            std::fs::read_to_string(rotated_path(&path, 1)).unwrap(),
            "0123456789"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
