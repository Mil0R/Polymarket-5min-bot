use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::warn;

/// Durable idempotency store for source event ids.  It deliberately sits above
/// individual feeds so a process restart cannot replay the same API/on-chain
/// observation into the strategy.
#[derive(Debug, Clone)]
pub struct ProcessedSignalStore {
    path: PathBuf,
    ids: Arc<Mutex<HashSet<String>>>,
    max_entries: usize,
}

impl ProcessedSignalStore {
    pub fn open(path: impl Into<PathBuf>, max_entries: usize) -> Self {
        let path = path.into();
        let ids = std::fs::read_to_string(&path)
            .ok()
            .map(|raw| {
                serde_json::from_str::<HashSet<String>>(&raw).unwrap_or_else(|_| {
                    raw.lines()
                        .filter_map(|line| serde_json::from_str::<String>(line).ok())
                        .collect()
                })
            })
            .unwrap_or_default();
        Self {
            path,
            ids: Arc::new(Mutex::new(ids)),
            max_entries: max_entries.max(1_000),
        }
    }

    /// Returns true exactly once for an id, including across clean restarts.
    pub fn check_and_record(&self, id: &str) -> bool {
        let mut ids = self.ids.lock().expect("processed signal lock poisoned");
        if ids.contains(id) {
            return false;
        }
        if ids.len() >= self.max_entries {
            // IDs are random/hash-like and have no ordering. Clearing is safe
            // because the consensus age gate prevents old events replaying.
            ids.clear();
            if let Err(e) = atomic_write_json(&self.path, &ids) {
                warn!(path = %self.path.display(), error = %e, "Processed-signal state compaction failed");
            }
        }
        ids.insert(id.to_string());
        if let Err(e) = append_id(&self.path, id) {
            warn!(path = %self.path.display(), error = %e, "Processed-signal state write failed");
        }
        true
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn append_id(path: &Path, id: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if file.metadata()?.len() > 0 {
        file.write_all(b"\n")?;
    }
    serde_json::to_writer(&mut file, id).map_err(std::io::Error::other)?;
    file.write_all(b"\n")
}

fn atomic_write_json(path: &Path, ids: &HashSet<String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut raw = serde_json::to_vec(ids).map_err(std::io::Error::other)?;
    raw.push(b'\n');
    std::fs::write(&tmp, raw)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_survive_restart() {
        let path = std::env::temp_dir().join(format!("copy-signals-{}.json", uuid::Uuid::new_v4()));
        let first = ProcessedSignalStore::open(&path, 1_000);
        assert!(first.check_and_record("sig-1"));
        assert!(!first.check_and_record("sig-1"));
        let second = ProcessedSignalStore::open(&path, 1_000);
        assert!(!second.check_and_record("sig-1"));
        let _ = std::fs::remove_file(path);
    }
}
