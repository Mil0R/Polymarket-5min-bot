use anyhow::Result;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::Metadata;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

pub const BINARY_COMPLEMENT_ARBITRAGE_LOG_TARGET: &str =
    "polypulse::strategy::binary_complement_arbitrage";
pub const LATE_RESOLUTION_WINNER_TRADING_LOG_TARGET: &str =
    "polypulse::strategy::late_resolution_winner_trading";
type LogLayer = Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>;

pub fn init_logger(quiet_stdout: bool) -> Result<()> {
    // Logger setup happens before Config::from_env(), so load .env here as well.
    dotenvy::dotenv().ok();
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let system_writer = if quiet_stdout {
        let path = std::env::var("LOG_FILE").unwrap_or_else(|_| "logs/system.log".to_string());
        BoxMakeWriter::new(Mutex::new(open_log_writer(path)?))
    } else if let Ok(path) = std::env::var("LOG_FILE") {
        BoxMakeWriter::new(Mutex::new(open_log_writer(path)?))
    } else {
        BoxMakeWriter::new(std::io::stdout)
    };

    let split_binary_complement_arbitrage =
        log_path_configured("BINARY_COMPLEMENT_ARBITRAGE_STRATEGY_LOG_FILE");
    let split_late_resolution_winner_trading =
        log_path_configured("LATE_RESOLUTION_WINNER_TRADING_STRATEGY_LOG_FILE");
    let split_wallet_copy_trading = log_path_configured("WALLET_COPY_TRADING_LOG_FILE");
    let mut log_layers: Vec<LogLayer> = vec![tracing_subscriber::fmt::layer()
        .with_writer(system_writer)
        .with_ansi(false)
        .with_filter(filter_fn(move |metadata| {
            !(split_binary_complement_arbitrage
                && is_binary_complement_arbitrage_metadata(metadata)
                || split_late_resolution_winner_trading
                    && is_late_resolution_winner_trading_metadata(metadata)
                || split_wallet_copy_trading && is_wallet_copy_trading_metadata(metadata))
        }))
        .boxed()];
    if let Some(layer) = strategy_log_layer(
        "BINARY_COMPLEMENT_ARBITRAGE_STRATEGY_LOG_FILE",
        is_binary_complement_arbitrage_metadata,
    )? {
        log_layers.push(layer);
    }
    if let Some(layer) = strategy_log_layer(
        "LATE_RESOLUTION_WINNER_TRADING_STRATEGY_LOG_FILE",
        is_late_resolution_winner_trading_metadata,
    )? {
        log_layers.push(layer);
    }
    if let Some(layer) = strategy_log_layer(
        "WALLET_COPY_TRADING_LOG_FILE",
        is_wallet_copy_trading_metadata,
    )? {
        log_layers.push(layer);
    }

    tracing_subscriber::registry()
        .with(log_layers)
        .with(env_filter)
        .init();

    Ok(())
}

fn strategy_log_layer(
    variable: &str,
    predicate: for<'a> fn(&Metadata<'a>) -> bool,
) -> Result<Option<LogLayer>> {
    let Some(path) = std::env::var(variable)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let writer = Mutex::new(open_log_writer(path)?);
    Ok(Some(
        tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_filter(filter_fn(predicate))
            .boxed(),
    ))
}

fn log_path_configured(variable: &str) -> bool {
    std::env::var(variable)
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

fn is_binary_complement_arbitrage_metadata(metadata: &Metadata<'_>) -> bool {
    let target = metadata.target();
    target == BINARY_COMPLEMENT_ARBITRAGE_LOG_TARGET
        || target.ends_with("::strategy_service")
        || target.ends_with("::monitor::arbitrage")
        || target.ends_with("::trading::executor")
}

fn is_late_resolution_winner_trading_metadata(metadata: &Metadata<'_>) -> bool {
    let target = metadata.target();
    target == LATE_RESOLUTION_WINNER_TRADING_LOG_TARGET
        || target.ends_with("::late_resolution_winner_trading")
        || target.ends_with("::late_resolution_winner_model")
}

fn is_wallet_copy_trading_metadata(metadata: &Metadata<'_>) -> bool {
    let target = metadata.target();
    target.ends_with("::wallet_copy_trading_embedded") || target.contains("::wallet_copy_trading")
}

struct SizeRotatingWriter {
    path: PathBuf,
    file: Option<File>,
    bytes_written: u64,
    max_bytes: u64,
    retention: usize,
}

impl SizeRotatingWriter {
    fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let max_bytes = std::env::var("LOG_ROTATE_MAX_BYTES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(10 * 1024 * 1024);
        let retention = std::env::var("LOG_ROTATE_FILES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(5);
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

    fn rotate_before(&mut self, incoming_bytes: usize) -> io::Result<()> {
        if self.retention == 0
            || self.bytes_written == 0
            || self.bytes_written.saturating_add(incoming_bytes as u64) <= self.max_bytes
        {
            return Ok(());
        }
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            drop(file);
        }
        rotate_log_file(&self.path, 1, self.retention)
            .map_err(|error| io::Error::other(error.to_string()))?;
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

impl Write for SizeRotatingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.rotate_before(buffer.len())?;
        let written = self
            .file
            .as_mut()
            .expect("rotating log writer must keep a file open")
            .write(buffer)?;
        self.bytes_written = self.bytes_written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .expect("rotating log writer must keep a file open")
            .flush()
    }
}

fn open_log_writer(path: impl AsRef<Path>) -> Result<SizeRotatingWriter> {
    SizeRotatingWriter::new(path)
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), index))
}

pub(crate) fn rotate_log_file(path: &Path, max_bytes: u64, retention: usize) -> Result<()> {
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

pub fn tui_enabled_from_env() -> bool {
    if std::env::var("PLAIN_LOGS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return false;
    }
    !std::env::var("TUI_ENABLED")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rotates_large_log_and_respects_retention() {
        let directory =
            std::env::temp_dir().join(format!("polypulse-log-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("bot.log");
        fs::write(&path, b"first generation").unwrap();
        fs::write(rotated_path(&path, 1), b"second generation").unwrap();
        fs::write(rotated_path(&path, 2), b"oldest generation").unwrap();

        rotate_log_file(&path, 1, 2).unwrap();
        assert_eq!(
            fs::read(rotated_path(&path, 1)).unwrap(),
            b"first generation"
        );
        assert_eq!(
            fs::read(rotated_path(&path, 2)).unwrap(),
            b"second generation"
        );

        let mut writer = open_log_writer(&path).unwrap();
        writer.write_all(b"new log").unwrap();
        drop(writer);
        assert_eq!(fs::read(&path).unwrap(), b"new log");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rotates_while_process_is_running() {
        let directory =
            std::env::temp_dir().join(format!("polypulse-live-log-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("bot.log");
        let mut writer = SizeRotatingWriter {
            path: path.clone(),
            file: Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap(),
            ),
            bytes_written: 0,
            max_bytes: 8,
            retention: 2,
        };
        writer.write_all(b"12345678").unwrap();
        writer.write_all(b"next").unwrap();
        writer.flush().unwrap();
        assert_eq!(fs::read(rotated_path(&path, 1)).unwrap(), b"12345678");
        assert_eq!(fs::read(&path).unwrap(), b"next");
        fs::remove_dir_all(directory).unwrap();
    }
}
