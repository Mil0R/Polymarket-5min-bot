//! Local, PAPER-only operator protocol. All acknowledgements follow durable writes.
use anyhow::{ensure, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const STUDY: &str = "winner-operator-paper-v2";
pub fn journal(baseline: &Path) -> PathBuf {
    baseline.with_file_name("operator-paper.ndjson")
}
pub fn socket(snapshot: &Path) -> PathBuf {
    snapshot.with_extension("control.sock")
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub revision: u64,
    pub entry_epoch: u64,
    pub paused: bool,
    pub budget: Decimal,
    pub pending_budget: Option<Decimal>,
    pub effective_window_ms: Option<i64>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            revision: 0,
            entry_epoch: 0,
            paused: true,
            budget: Decimal::from(5),
            pending_budget: None,
            effective_window_ms: None,
        }
    }
}
pub fn valid_budget(value: Decimal) -> Result<()> {
    ensure!(
        value >= Decimal::ONE && value <= Decimal::from(100) && value.scale() <= 2,
        "PAPER budget must be 1.00–100.00 USDC, at most two decimals"
    );
    Ok(())
}
pub fn initial_capital() -> Decimal {
    Decimal::from(100)
}
pub fn valid_deposit(value: Decimal) -> Result<()> {
    ensure!(
        value > Decimal::ZERO && value <= Decimal::from(1_000_000) && value.scale() <= 2,
        "PAPER deposit must be 0.01–1,000,000.00 USDC, at most two decimals"
    );
    Ok(())
}
pub fn validate_funding(
    settings: &Settings,
    command: &Command,
    amount: Decimal,
    revision: u64,
) -> Result<()> {
    valid_deposit(amount)?;
    ensure!(
        uuid::Uuid::parse_str(&command.id).is_ok()
            && command.expected_revision == settings.revision
            && settings.revision.checked_add(1) == Some(revision)
            && matches!(command.action, Action::Deposit { amount: value } if value == amount),
        "invalid PAPER funding command/revision"
    );
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Pause,
    Resume,
    SetBudget { amount: Decimal },
    Deposit { amount: Decimal },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub id: String,
    pub run_id: String,
    pub journal: String,
    pub expected_revision: u64,
    pub action: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub journal: String,
    pub settings: Settings,
    pub failed: bool,
    pub inflight: usize,
    pub pending_fills: usize,
    pub cash: Decimal,
    #[serde(default = "initial_capital")]
    pub capital: Decimal,
    #[serde(default)]
    pub deposits: Decimal,
    #[serde(default)]
    pub funding_supported: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reply {
    pub id: String,
    pub accepted: bool,
    pub message: String,
    pub status: Option<Status>,
}

pub fn send(path: &Path, command: &Command) -> Result<Reply> {
    #[cfg(unix)]
    {
        use std::io::{BufRead, BufReader, Read, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(socket(path))?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(1)))?;
        let mut bytes = serde_json::to_vec(command)?;
        bytes.push(b'\n');
        stream.write_all(&bytes)?;
        let mut bytes = Vec::new();
        BufReader::new(stream)
            .take(8193)
            .read_until(b'\n', &mut bytes)?;
        ensure!(
            bytes.len() <= 8192 && bytes.last() == Some(&b'\n'),
            "invalid control reply"
        );
        let reply: Reply = serde_json::from_slice(&bytes)?;
        ensure!(
            reply.id == command.id,
            "control acknowledgement ID mismatch"
        );
        Ok(reply)
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("operator controls require Unix sockets")
    }
}
