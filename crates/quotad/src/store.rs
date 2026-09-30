//! Bounded in-memory ring of snapshots plus optional append-only JSONL.
//!
//! The ring is the only history `pace` / `can_start` see. JSONL is a debug
//! trail and is never read back in v0 (avoids loading unbounded files).

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};

use quota_core::types::{ProviderId, Snapshot};
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

    /// Overwrite the newest entry instead of growing the ring. A passive
    /// source re-confirming the same numbers must not evict other providers'
    /// history.
    pub fn replace_latest(&mut self, snap: Snapshot) {
        match self.ring.back_mut() {
            Some(last) => *last = snap,
            None => self.push_memory(snap),
        }
    }

    /// Drop every in-memory reading of `provider`, so no later pace or
    /// `can_start` answer samples it. The JSONL trail is left as written.
    pub fn forget(&mut self, provider: ProviderId) {
        for snap in &mut self.ring {
            snap.providers.retain(|p| p.provider != provider);
        }
    }

    pub fn latest(&self) -> Option<&Snapshot> {
        self.ring.back()
    }

    /// Borrow the ring (no `Snapshot` clones) for `pace` / `can_start`.
    pub fn history_ref(&self) -> &VecDeque<Snapshot> {
        &self.ring
    }
}

fn append_jsonl(path: &Path, snap: &Snapshot) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        quota_core::ensure_private_dir(dir)?;
    }
    let mut f = quota_core::open_private_append(path)?;
    serde_json::to_writer(&mut f, snap).map_err(std::io::Error::other)?;
    f.write_all(b"\n")?;
    quota_core::chmod_private_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_core::types::{AdapterError, ProviderSnapshot};

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
        assert_eq!(s.history_ref().len(), 2);
        assert_eq!(s.latest().unwrap().fetched_at, 3);
    }
}
