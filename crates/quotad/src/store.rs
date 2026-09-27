//! Bounded in-memory ring of snapshots plus optional append-only JSONL.
//!
//! The ring is the only history `pace` / `can_start` see. JSONL is a debug
//! trail and is never read back in v0 (avoids loading unbounded files).

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use quota_core::types::Snapshot;
use quota_core::Config;

pub struct Store {
    ring: VecDeque<Snapshot>,
    cap: usize,
    history_path: Option<PathBuf>,
}

impl Store {
    pub fn new(cfg: &Config) -> Self {
        Self {
            ring: VecDeque::with_capacity(cfg.ring_capacity()),
            cap: cfg.ring_capacity(),
            history_path: cfg.history_file(),
        }
    }

    pub fn push(&mut self, snap: Snapshot) {
        if let Some(path) = &self.history_path {
            let _ = append_jsonl(path, &snap);
        }
        self.push_memory(snap);
    }

    /// In-memory only (CodexBar history seed — do not copy into our JSONL).
    pub fn push_memory(&mut self, snap: Snapshot) {
        if self.ring.len() == self.cap {
            self.ring.pop_front();
        }
        self.ring.push_back(snap);
    }

    pub fn latest(&self) -> Option<&Snapshot> {
        self.ring.back()
    }

    pub fn history(&self) -> Vec<Snapshot> {
        self.ring.iter().cloned().collect()
    }
}

fn append_jsonl(path: &PathBuf, snap: &Snapshot) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut f, snap).map_err(std::io::Error::other)?;
    f.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_core::types::{AdapterError, ProviderId, ProviderSnapshot};

    #[test]
    fn ring_drops_oldest() {
        let cfg = Config {
            ring_capacity: 8,
            history: false,
            ..Config::default()
        };
        let mut s = Store::new(&cfg);
        // Force tiny cap.
        s.cap = 2;
        s.push(Snapshot::new(
            1,
            vec![ProviderSnapshot::unavailable(
                ProviderId::Codex,
                AdapterError::new("x", "x"),
            )],
        ));
        s.push(Snapshot::new(
            2,
            vec![ProviderSnapshot::unavailable(
                ProviderId::Codex,
                AdapterError::new("y", "y"),
            )],
        ));
        s.push(Snapshot::new(
            3,
            vec![ProviderSnapshot::unavailable(
                ProviderId::Codex,
                AdapterError::new("z", "z"),
            )],
        ));
        assert_eq!(s.history().len(), 2);
        assert_eq!(s.latest().unwrap().fetched_at, 3);
    }
}
