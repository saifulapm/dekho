//! Which release played last time, so re-watching and resuming start from the
//! pieces already on disk.
//!
//! The resolution walk is deterministic-ish, but not stable enough to promise
//! that resuming a film lands on the same release whose pieces are cached: last
//! week's run may have downgraded to attempt three, and replaying attempts one
//! and two costs two probe budgets before the cache gets a chance. Remembering
//! the winner's info hash per title lets `resolve` try it *first*, where the
//! probe's instant-start path can recognise the cached slab and skip straight
//! to mpv.
//!
//! Deliberately only a hint: the remembered release is promoted within the
//! shortlist, not exempted from it. A release the adapted bitrate ceiling now
//! rejects stays rejected — a cached head does not make the remaining hour of
//! an over-heavy stream sustainable.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::torrentio::Candidate;

/// Bumped only if the shape changes in a way an older dekho cannot read.
const VERSION: u32 = 1;

/// Entries kept, dropping the least recently updated — same reasoning as the
/// history cap, and the whole file is rewritten on every save.
const MAX_ENTRIES: usize = 200;

/// Where the release memory lives, next to `history.json`.
pub fn path() -> PathBuf {
    crate::xdg::state_home().join("dekho").join("releases.json")
}

/// One remembered winner. The key is Torrentio's stream id — `tt123` for a
/// movie, `tt123:2:5` for an episode — because that is exactly the identity
/// `resolve` already looks candidates up by.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Remembered {
    pub key: String,
    /// Lowercased btih info hash.
    pub info_hash: String,
    pub updated: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Replay {
    pub version: u32,
    #[serde(default)]
    pub items: Vec<Remembered>,
}

impl Replay {
    pub fn empty() -> Self {
        Self {
            version: VERSION,
            items: Vec::new(),
        }
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(Self::empty)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("the replay path has no parent directory"))?;
        std::fs::create_dir_all(dir)?;
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn recall(&self, key: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|r| r.key == key)
            .map(|r| r.info_hash.as_str())
    }

    pub fn remember(&mut self, key: &str, info_hash: &str, now: u64) {
        self.items.retain(|r| r.key != key);
        self.items.insert(
            0,
            Remembered {
                key: key.to_string(),
                info_hash: info_hash.to_ascii_lowercase(),
                updated: now,
            },
        );
        self.items.sort_by_key(|r| std::cmp::Reverse(r.updated));
        self.items.truncate(MAX_ENTRIES);
    }
}

/// Move the remembered release to the front of the attempt order, if it is
/// still in it. Order among the rest is preserved.
pub fn promote<'a>(order: Vec<&'a Candidate>, info_hash: Option<&str>) -> Vec<&'a Candidate> {
    let Some(hash) = info_hash else {
        return order;
    };
    let Some(pos) = order
        .iter()
        .position(|c| crate::sources::info_hash_of(&c.magnet) == hash)
    else {
        return order;
    };
    let mut order = order;
    let chosen = order.remove(pos);
    order.insert(0, chosen);
    order
}

/// Shared state with best-effort persistence, like `LinkStore`.
pub struct ReplayStore {
    path: PathBuf,
    inner: Mutex<Replay>,
}

impl ReplayStore {
    pub fn open(path: PathBuf) -> Self {
        let replay = Replay::load(&path);
        Self {
            path,
            inner: Mutex::new(replay),
        }
    }

    pub fn recall(&self, key: &str) -> Option<String> {
        let replay = self.inner.lock().ok()?;
        replay.recall(key).map(str::to_string)
    }

    pub fn remember(&self, key: &str, info_hash: &str) {
        let Ok(mut replay) = self.inner.lock() else {
            return;
        };
        replay.remember(key, info_hash, crate::history::now());
        let _ = replay.save(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrentio::{build_magnet, Quality};

    fn candidate(hash: &str) -> Candidate {
        Candidate {
            quality: Quality::P1080,
            title: hash.to_string(),
            size_bytes: Some(1_000_000_000),
            seeders: 10,
            file_idx: None,
            magnet: build_magnet(hash, "x", None),
            audio: crate::audio::Audio::default(),
        }
    }

    #[test]
    fn remember_and_recall_round_trip() {
        let mut r = Replay::empty();
        r.remember("tt0137523", "ABC123", 100);
        assert_eq!(r.recall("tt0137523"), Some("abc123"), "hash is lowercased");
        assert_eq!(r.recall("tt0000001"), None);
    }

    #[test]
    fn remembering_again_replaces_the_old_winner() {
        let mut r = Replay::empty();
        r.remember("tt1:2:5", "aaa", 100);
        r.remember("tt1:2:5", "bbb", 200);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.recall("tt1:2:5"), Some("bbb"));
    }

    #[test]
    fn the_store_is_capped_dropping_the_least_recent() {
        let mut r = Replay::empty();
        for i in 0..(MAX_ENTRIES + 20) {
            r.remember(&format!("tt{i}"), "aaa", i as u64);
        }
        assert_eq!(r.items.len(), MAX_ENTRIES);
        assert!(r.recall("tt0").is_none(), "the oldest fell off");
        assert!(r.recall(&format!("tt{}", MAX_ENTRIES + 19)).is_some());
    }

    #[test]
    fn promote_moves_the_remembered_release_first() {
        let (a, b, c) = (candidate("aaa"), candidate("bbb"), candidate("ccc"));
        let order = promote(vec![&a, &b, &c], Some("bbb"));
        let hashes: Vec<&str> = order.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(hashes, vec!["bbb", "aaa", "ccc"], "rest keep their order");
    }

    #[test]
    fn promote_without_a_match_changes_nothing() {
        let (a, b) = (candidate("aaa"), candidate("bbb"));
        let order = promote(vec![&a, &b], Some("zzz"));
        assert_eq!(order.len(), 2);
        assert_eq!(order[0].title, "aaa");
    }

    #[test]
    fn promote_without_a_memory_changes_nothing() {
        let a = candidate("aaa");
        let order = promote(vec![&a], None);
        assert_eq!(order.len(), 1);
    }
}
