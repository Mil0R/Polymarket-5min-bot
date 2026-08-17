use anyhow::Result;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub fn init_logger(quiet_stdout: bool) -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if quiet_stdout {
        let path = std::env::var("LOG_FILE").unwrap_or_else(|_| "bot.log".to_string());
        let file = open_log_writer(&path)?;
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(file)
                    .with_ansi(false),
            )
            .init();
    } else if let Ok(path) = std::env::var("LOG_FILE") {
        let file = open_log_writer(&path)?;
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(file)
                    .with_ansi(false),
            )
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    Ok(())
}

fn open_log_writer(path: impl AsRef<Path>) -> Result<File> {
    let max_bytes = std::env::var("LOG_ROTATE_MAX_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10 * 1024 * 1024);
    let retention = std::env::var("LOG_ROTATE_FILES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5);
    rotate_log_file(path.as_ref(), max_bytes, retention)?;
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
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
}
