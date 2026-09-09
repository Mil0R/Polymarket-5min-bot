//! Append-only order-pair state journal for crash/restart auditing.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JournalState {
    BothFilled,
    PartiallyFilled,
    OneSided,
    BothFailed,
    Recovering,
    Recovered,
    ManualReview,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalPairSnapshot {
    #[serde(default)]
    pub market_id: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub yes_order_id: String,
    pub no_order_id: String,
    pub yes_size: String,
    pub no_size: String,
    pub yes_price: String,
    pub no_price: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalRecoverySnapshot {
    pub token_id: String,
    pub opposite_token_id: String,
    pub target_amount: String,
    pub entry_price: String,
    pub take_profit_price: String,
    pub stop_loss_price: String,
    pub market_display: String,
    pub order_id: Option<String>,
    pub pending_sell_amount: String,
    pub accounted_sell_fill: String,
    /// Explicit HedgeMonitor lifecycle state. Empty on journals written by
    /// older versions and interpreted as `open` during recovery.
    #[serde(default)]
    pub close_state: String,
}

impl JournalState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::BothFilled | Self::BothFailed | Self::Recovered)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEvent {
    pub timestamp: String,
    pub pair_id: String,
    pub market_id: String,
    pub state: JournalState,
    pub yes_filled: String,
    pub no_filled: String,
    pub detail: Option<String>,
    #[serde(default)]
    pub pair: Option<JournalPairSnapshot>,
    #[serde(default)]
    pub recovery: Option<JournalRecoverySnapshot>,
}

const JOURNAL_VERSION: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct JournalEnvelope {
    version: u8,
    checksum: String,
    event: JournalEvent,
}

pub struct StateJournal {
    path: PathBuf,
    writer: Mutex<Option<File>>,
    latest: Mutex<HashMap<String, JournalEvent>>,
}

impl StateJournal {
    /// Isolated research state: never opens or changes a strategy's disk journal.
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            writer: Mutex::new(None),
            latest: Mutex::new(HashMap::new()),
        }
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create journal directory {}", parent.display())
            })?;
        }
        let latest = if path.exists() {
            read_latest_events(&path)?
        } else {
            HashMap::new()
        };
        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open state journal {}", path.display()))?;
        Ok(Self {
            path,
            writer: Mutex::new(Some(writer)),
            latest: Mutex::new(latest),
        })
    }

    pub fn probe_writable(&self) -> Result<()> {
        if let Some(writer) = self
            .writer
            .lock()
            .expect("state journal lock poisoned")
            .as_ref()
        {
            writer
                .sync_data()
                .context("state journal sync probe failed")?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, event: &JournalEvent) -> Result<()> {
        let checksum = checksum_event(event)?;
        let encoded = serde_json::to_string(&JournalEnvelope {
            version: JOURNAL_VERSION,
            checksum,
            event: event.clone(),
        })?;
        let mut writer = self.writer.lock().expect("state journal lock poisoned");
        if let Some(writer) = writer.as_mut() {
            writeln!(writer, "{encoded}")?;
            writer.flush()?;
            writer.sync_data()?;
        }
        self.latest
            .lock()
            .expect("state journal cache lock poisoned")
            .insert(event.pair_id.clone(), event.clone());
        Ok(())
    }

    pub fn unresolved(&self) -> Result<Vec<JournalEvent>> {
        Ok(self
            .latest_events()?
            .into_values()
            .filter(|event| !event.state.is_terminal())
            .collect())
    }

    pub fn latest_event(&self, pair_id: &str) -> Result<Option<JournalEvent>> {
        Ok(self
            .latest
            .lock()
            .expect("state journal cache lock poisoned")
            .get(pair_id)
            .cloned())
    }

    pub fn has_unresolved(&self) -> bool {
        self.latest
            .lock()
            .expect("state journal cache lock poisoned")
            .values()
            .any(|event| !event.state.is_terminal())
    }

    pub fn pair_is_unresolved(&self, pair_id: &str) -> bool {
        self.latest
            .lock()
            .expect("state journal cache lock poisoned")
            .get(pair_id)
            .is_some_and(|event| !event.state.is_terminal())
    }

    fn latest_events(&self) -> Result<HashMap<String, JournalEvent>> {
        Ok(self
            .latest
            .lock()
            .expect("state journal cache lock poisoned")
            .clone())
    }
}

fn read_latest_events(path: &Path) -> Result<HashMap<String, JournalEvent>> {
    let reader = BufReader::new(File::open(path)?);
    let lines: Vec<String> = reader.lines().collect::<std::io::Result<_>>()?;
    let last_record = lines.iter().rposition(|line| !line.trim().is_empty());
    let mut latest = HashMap::new();
    for (line_number, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = match decode_record(line) {
            Ok(event) => event,
            Err(error) if Some(line_number) == last_record => {
                quarantine_corrupt_tail(path, &lines, line_number)?;
                tracing::warn!(
                    path = %path.display(),
                    line = line_number + 1,
                    %error,
                    "Quarantined corrupt state journal tail"
                );
                break;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "invalid state journal record at {}:{}",
                        path.display(),
                        line_number + 1
                    )
                })
            }
        };
        latest.insert(event.pair_id.clone(), event);
    }
    Ok(latest)
}

fn checksum_event(event: &JournalEvent) -> Result<String> {
    let encoded = serde_json::to_vec(event)?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

fn decode_record(line: &str) -> Result<JournalEvent> {
    let value: serde_json::Value = serde_json::from_str(line)?;
    if value.get("version").is_none() {
        return Ok(serde_json::from_value(value)?);
    }
    let envelope: JournalEnvelope = serde_json::from_value(value)?;
    if envelope.version != JOURNAL_VERSION {
        anyhow::bail!("unsupported journal version {}", envelope.version);
    }
    let actual = checksum_event(&envelope.event)?;
    if actual != envelope.checksum {
        anyhow::bail!("journal checksum mismatch");
    }
    Ok(envelope.event)
}

fn quarantine_corrupt_tail(path: &Path, lines: &[String], corrupt_index: usize) -> Result<PathBuf> {
    let quarantine = PathBuf::from(format!(
        "{}.corrupt-{}",
        path.display(),
        chrono::Utc::now().timestamp_millis()
    ));
    let corrupt = lines[corrupt_index..].join("\n");
    std::fs::write(&quarantine, corrupt.as_bytes())?;
    let mut valid = lines[..corrupt_index].join("\n");
    if !valid.is_empty() {
        valid.push('\n');
    }
    let mut repaired = OpenOptions::new().write(true).truncate(true).open(path)?;
    repaired.write_all(valid.as_bytes())?;
    repaired.sync_all()?;
    Ok(quarantine)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(pair: &str, state: JournalState) -> JournalEvent {
        JournalEvent {
            timestamp: "2026-01-01T00:00:00Z".into(),
            pair_id: pair.into(),
            market_id: "market".into(),
            state,
            yes_filled: "1".into(),
            no_filled: "0".into(),
            detail: None,
            pair: None,
            recovery: None,
        }
    }

    #[test]
    fn isolated_memory_journal_tracks_state_without_a_file() {
        let journal = StateJournal::in_memory();
        assert!(journal.path().as_os_str().is_empty());
        journal.probe_writable().unwrap();
        journal
            .append(&event("paper", JournalState::OneSided))
            .unwrap();
        assert!(journal.has_unresolved());
        journal
            .append(&event("paper", JournalState::Recovered))
            .unwrap();
        assert!(!journal.has_unresolved());
        assert!(journal.writer.lock().unwrap().is_none());
    }

    #[test]
    fn replay_returns_only_latest_unresolved_pairs() {
        let path =
            std::env::temp_dir().join(format!("polypulse-journal-{}.ndjson", uuid::Uuid::new_v4()));
        let journal = StateJournal::open(&path).unwrap();
        journal.append(&event("a", JournalState::OneSided)).unwrap();
        journal
            .append(&event("a", JournalState::Recovered))
            .unwrap();
        journal
            .append(&event("b", JournalState::Recovering))
            .unwrap();
        let unresolved = journal.unresolved().unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].pair_id, "b");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn caches_and_reloads_recovery_snapshots() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-recovery-journal-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let journal = StateJournal::open(&path).unwrap();
        journal
            .append(&JournalEvent {
                timestamp: "2026-01-01T00:00:00Z".into(),
                pair_id: "pair".into(),
                market_id: "0x01".into(),
                state: JournalState::Recovering,
                yes_filled: "3".into(),
                no_filled: "0".into(),
                detail: Some("resting".into()),
                pair: Some(JournalPairSnapshot {
                    market_id: "0x01".into(),
                    yes_token_id: "1".into(),
                    no_token_id: "2".into(),
                    yes_order_id: "yes-order".into(),
                    no_order_id: String::new(),
                    yes_size: "3".into(),
                    no_size: "3".into(),
                    yes_price: "0.4".into(),
                    no_price: "0.5".into(),
                }),
                recovery: Some(JournalRecoverySnapshot {
                    token_id: "1".into(),
                    opposite_token_id: "2".into(),
                    target_amount: "3".into(),
                    entry_price: "0.4".into(),
                    take_profit_price: "0.42".into(),
                    stop_loss_price: "0.38".into(),
                    market_display: "market".into(),
                    order_id: Some("sell-order".into()),
                    pending_sell_amount: "2".into(),
                    accounted_sell_fill: "1".into(),
                    close_state: "resting".into(),
                }),
            })
            .unwrap();
        assert!(journal.has_unresolved());
        assert_eq!(
            journal
                .latest_event("pair")
                .unwrap()
                .unwrap()
                .recovery
                .unwrap()
                .order_id
                .as_deref(),
            Some("sell-order")
        );
        drop(journal);

        let reloaded = StateJournal::open(&path).unwrap();
        assert!(reloaded.has_unresolved());
        assert_eq!(reloaded.unresolved().unwrap().len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn writes_versioned_checksum_envelope_and_reads_legacy_records() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-versioned-journal-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let legacy = serde_json::to_string(&event("legacy", JournalState::OneSided)).unwrap();
        std::fs::write(&path, format!("{legacy}\n")).unwrap();
        let journal = StateJournal::open(&path).unwrap();
        journal
            .append(&event("versioned", JournalState::Recovering))
            .unwrap();
        drop(journal);
        let records = std::fs::read_to_string(&path).unwrap();
        let last: serde_json::Value =
            serde_json::from_str(records.lines().last().unwrap()).unwrap();
        assert_eq!(last["version"], JOURNAL_VERSION);
        assert!(last["checksum"]
            .as_str()
            .is_some_and(|value| value.len() == 64));
        let reloaded = StateJournal::open(&path).unwrap();
        assert_eq!(reloaded.unresolved().unwrap().len(), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn quarantines_partial_tail_and_preserves_valid_prefix() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-corrupt-tail-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let journal = StateJournal::open(&path).unwrap();
        journal
            .append(&event("safe", JournalState::OneSided))
            .unwrap();
        drop(journal);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"version\":1,\"partial\"")
            .unwrap();

        let reopened = StateJournal::open(&path).unwrap();
        assert_eq!(reopened.unresolved().unwrap()[0].pair_id, "safe");
        let file_name = path.file_name().unwrap().to_string_lossy();
        let quarantined = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|candidate| {
                candidate.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with(&format!("{file_name}.corrupt-"))
                })
            })
            .expect("corrupt tail quarantine");
        assert!(std::fs::read_to_string(&quarantined)
            .unwrap()
            .contains("partial"));
        drop(reopened);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(quarantined).unwrap();
    }

    #[test]
    fn rejects_checksum_corruption_in_the_middle() {
        let path = std::env::temp_dir().join(format!(
            "polypulse-corrupt-middle-{}.ndjson",
            uuid::Uuid::new_v4()
        ));
        let valid = event("valid", JournalState::OneSided);
        let envelope = JournalEnvelope {
            version: JOURNAL_VERSION,
            checksum: "0".repeat(64),
            event: event("bad", JournalState::OneSided),
        };
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&envelope).unwrap(),
            serde_json::to_string(&valid).unwrap()
        );
        std::fs::write(&path, content).unwrap();
        let error = match StateJournal::open(&path) {
            Ok(_) => panic!("corrupt middle record should fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("invalid state journal record"));
        std::fs::remove_file(path).unwrap();
    }
}
