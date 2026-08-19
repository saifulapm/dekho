//! Every indexer we ask, merged into one candidate list.
//!
//! Torrentio alone is curated and deduplicated, which is normally a virtue and
//! actively unhelpful when hunting dual audio — for `3 Idiots` it returns 14
//! streams where apibay returns 53, and the Hindi/English releases sit mostly
//! in the tail it drops.
//!
//! The two are queried concurrently and merged on info hash. Neither is allowed
//! to fail the lookup: an indexer that is down, rate-limiting or reshaping its
//! JSON costs us its results, not the whole search.

use std::collections::HashMap;

use anyhow::Result;

use crate::apibay::ApiBay;
use crate::torrentio::{Candidate, Torrentio};

pub struct Sources {
    torrentio: Torrentio,
    apibay: ApiBay,
}

/// Where a candidate came from, and how many each contributed.
#[derive(Clone, Copy, Debug, Default)]
pub struct SourceCounts {
    pub torrentio: usize,
    pub apibay: usize,
    /// Present in both, counted once in the merged list.
    pub shared: usize,
}

pub struct Lookup {
    pub candidates: Vec<Candidate>,
    pub counts: SourceCounts,
    /// Indexers that errored, for a one-line warning rather than a failure.
    pub failed: Vec<&'static str>,
}

impl Sources {
    pub fn new() -> Result<Self> {
        Ok(Self {
            torrentio: Torrentio::new()?,
            apibay: ApiBay::new()?,
        })
    }

    pub async fn candidates(
        &self,
        imdb_id: &str,
        kind: &str,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Lookup {
        let (t, a) = tokio::join!(
            self.torrentio.candidates(imdb_id, kind, season, episode),
            self.apibay.candidates(imdb_id, season, episode),
        );

        let mut failed = Vec::new();
        let torrentio = t.unwrap_or_else(|_| {
            failed.push("Torrentio");
            Vec::new()
        });
        let apibay = a.unwrap_or_else(|_| {
            failed.push("apibay");
            Vec::new()
        });

        let mut counts = SourceCounts {
            torrentio: torrentio.len(),
            apibay: apibay.len(),
            shared: 0,
        };
        let (candidates, shared) = merge(torrentio, apibay);
        counts.shared = shared;

        Lookup {
            candidates,
            counts,
            failed,
        }
    }
}

/// Merge two candidate lists on info hash, preferring Torrentio's copy.
///
/// Torrentio wins ties because its metadata is richer: it carries `fileIdx`
/// (which is how a season pack resolves to one episode without guessing) and
/// language flags. Where apibay knows a language Torrentio did not, that is
/// folded in rather than dropped — the two describe the same torrent, so the
/// union is more accurate than either.
fn merge(torrentio: Vec<Candidate>, apibay: Vec<Candidate>) -> (Vec<Candidate>, usize) {
    let mut by_hash: HashMap<String, Candidate> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut shared = 0;

    for c in torrentio {
        let key = info_hash_of(&c.magnet);
        order.push(key.clone());
        by_hash.insert(key, c);
    }

    for c in apibay {
        let key = info_hash_of(&c.magnet);
        match by_hash.get_mut(&key) {
            Some(existing) => {
                shared += 1;
                existing.audio.hindi |= c.audio.hindi;
                existing.audio.english |= c.audio.english;
                existing.audio.multi_marker |= c.audio.multi_marker;
                existing.audio.others.extend(c.audio.others.iter().copied());
                // apibay reports exact byte counts; Torrentio rounds to a
                // human-readable figure, and the exact one sizes buffers better.
                if c.size_bytes.is_some() {
                    existing.size_bytes = c.size_bytes;
                }
                existing.seeders = existing.seeders.max(c.seeders);
            }
            None => {
                order.push(key.clone());
                by_hash.insert(key, c);
            }
        }
    }

    let merged: Vec<Candidate> = order
        .into_iter()
        .filter_map(|k| by_hash.remove(&k))
        .collect();
    (merged, shared)
}

/// The `btih` hash out of a magnet, lowercased so the two indexers' casing
/// does not defeat deduplication — Torrentio emits lowercase, apibay uppercase.
fn info_hash_of(magnet: &str) -> String {
    magnet
        .split("xt=urn:btih:")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .unwrap_or(magnet)
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Audio;
    use crate::torrentio::{build_magnet, Quality};

    fn candidate(hash: &str, name: &str, seeders: u32, size: Option<u64>) -> Candidate {
        Candidate {
            quality: Quality::P1080,
            title: name.into(),
            size_bytes: size,
            seeders,
            file_idx: None,
            magnet: build_magnet(hash, name, None),
            audio: crate::audio::detect(name),
        }
    }

    #[test]
    fn info_hash_is_extracted_and_lowercased() {
        let m = build_magnet("ABCDEF123", "Name", None);
        assert_eq!(info_hash_of(&m), "abcdef123");
    }

    #[test]
    fn the_same_torrent_from_both_sources_appears_once() {
        let t = vec![candidate("aaa", "Movie 1080p", 10, Some(100))];
        let a = vec![candidate("AAA", "Movie 1080p", 20, Some(200))];
        let (merged, shared) = merge(t, a);
        assert_eq!(
            merged.len(),
            1,
            "case-different hashes are the same torrent"
        );
        assert_eq!(shared, 1);
    }

    #[test]
    fn merging_keeps_the_better_seeder_count_and_exact_size() {
        let t = vec![candidate("aaa", "Movie 1080p", 10, Some(100))];
        let a = vec![candidate("aaa", "Movie 1080p", 55, Some(2_691_958_652))];
        let (merged, _) = merge(t, a);
        assert_eq!(merged[0].seeders, 55);
        assert_eq!(merged[0].size_bytes, Some(2_691_958_652));
    }

    #[test]
    fn language_knowledge_from_either_source_is_unioned() {
        // Torrentio flagged only Hindi; apibay's name says dual audio English.
        let mut only_hindi = candidate("aaa", "Movie 1080p", 10, Some(100));
        only_hindi.audio = Audio {
            hindi: true,
            ..Audio::default()
        };
        let a = vec![candidate(
            "aaa",
            "Movie 1080p Dual Audio Hindi English",
            10,
            Some(100),
        )];
        let (merged, _) = merge(vec![only_hindi], a);
        assert!(merged[0].audio.hindi);
        assert!(merged[0].audio.english, "apibay's English claim survives");
        assert_eq!(merged[0].audio.dual(), crate::audio::Dual::Yes);
    }

    #[test]
    fn releases_unique_to_apibay_are_added() {
        let t = vec![candidate("aaa", "Movie 1080p", 10, Some(100))];
        let a = vec![
            candidate("aaa", "Movie 1080p", 10, Some(100)),
            candidate("bbb", "Movie Dual Audio 1080p", 5, Some(100)),
        ];
        let (merged, shared) = merge(t, a);
        assert_eq!(merged.len(), 2);
        assert_eq!(shared, 1);
    }

    #[test]
    fn torrentio_entries_keep_their_leading_order() {
        let t = vec![
            candidate("aaa", "First", 10, None),
            candidate("bbb", "Second", 10, None),
        ];
        let a = vec![candidate("ccc", "Third", 10, None)];
        let (merged, _) = merge(t, a);
        let titles: Vec<&str> = merged.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["First", "Second", "Third"]);
    }

    #[test]
    fn merging_empty_lists_is_not_an_error() {
        let (merged, shared) = merge(Vec::new(), Vec::new());
        assert!(merged.is_empty());
        assert_eq!(shared, 0);
    }
}
