//! apibay — The Pirate Bay's public JSON API, used as a second indexer.
//!
//! Torrentio is curated and deduplicated, which is usually a virtue and is a
//! problem for dual-audio hunting: for `3 Idiots` it returns 14 streams where
//! apibay returns 53, and the Hindi/English releases are mostly in the tail it
//! drops.
//!
//! Queried by IMDB id, not by title. apibay stores an `imdb` field per torrent
//! and matches on it, so results are exact — a text search for "3 idiots"
//! returns SpongeBob's "Survival of the Idiots" near the top, and auto-playing
//! that would be worse than finding nothing.
//!
//! The catch is that TV has no episode granularity here: a series id returns
//! season packs and loose episodes together, so the caller's season/episode is
//! matched against the release name.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::audio;
use crate::engine::matches_episode;
use crate::torrentio::{build_magnet, parse_quality, Candidate};

const BASE: &str = "https://apibay.org";

/// apibay signals "nothing found" with a single row rather than an empty list,
/// and that row carries an all-zero hash.
const EMPTY_HASH: &str = "0000000000000000000000000000000000000000";

#[derive(Deserialize)]
struct RawTorrent {
    #[serde(default)]
    name: String,
    #[serde(default)]
    info_hash: String,
    #[serde(default)]
    seeders: String,
    #[serde(default)]
    size: String,
    #[serde(default)]
    imdb: String,
}

pub struct ApiBay {
    http: reqwest::Client,
}

impl ApiBay {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("dekho/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("building the apibay HTTP client")?;
        Ok(Self { http })
    }

    /// Releases for a title. For TV, `season`/`episode` filter the results by
    /// name, keeping season packs (which contain the episode) as well as
    /// single-episode releases.
    pub async fn candidates(
        &self,
        imdb_id: &str,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Result<Vec<Candidate>> {
        let url = format!("{BASE}/q.php?q={imdb_id}");
        let res = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting apibay")?;
        anyhow::ensure!(
            res.status().is_success(),
            "apibay returned HTTP {}",
            res.status()
        );
        let rows: Vec<RawTorrent> = res.json().await.context("decoding apibay response")?;
        Ok(parse_rows(rows, imdb_id, season, episode))
    }
}

fn parse_rows(
    rows: Vec<RawTorrent>,
    imdb_id: &str,
    season: Option<u32>,
    episode: Option<u32>,
) -> Vec<Candidate> {
    rows.into_iter()
        .filter(|r| !r.info_hash.is_empty() && r.info_hash != EMPTY_HASH)
        // apibay matches loosely enough that a stray title can come back;
        // insist the row is actually tagged with the id we asked for.
        .filter(|r| r.imdb.is_empty() || r.imdb.eq_ignore_ascii_case(imdb_id))
        .filter(|r| keeps_episode(&r.name, season, episode))
        .map(|r| {
            let audio = audio::detect(&r.name);
            Candidate {
                quality: parse_quality(&r.name),
                size_bytes: r.size.parse::<u64>().ok().filter(|s| *s > 0),
                seeders: r.seeders.parse::<u32>().unwrap_or(0),
                // apibay has no per-file index, so a season pack is resolved by
                // filename once the torrent's metadata arrives.
                file_idx: None,
                magnet: build_magnet(&r.info_hash, &r.name, None),
                audio,
                title: r.name,
            }
        })
        .collect()
}

/// Whether a release is worth considering for the requested episode.
///
/// Keeps anything naming the episode, plus season packs for that season, plus
/// complete-series packs. Rejects releases that clearly belong to a different
/// season, which is what stops a `Season 4` pack being offered for `S02E01`.
fn keeps_episode(name: &str, season: Option<u32>, episode: Option<u32>) -> bool {
    let (Some(s), Some(e)) = (season, episode) else {
        return true;
    };
    if matches_episode(name, s, e) {
        return true;
    }

    let flat: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect();
    let padded = format!(
        " {} ",
        flat.split_whitespace().collect::<Vec<_>>().join(" ")
    );

    // A pack for exactly this season.
    for form in [
        format!(" season {s} "),
        format!(" s{s:02} "),
        format!(" s{s} "),
    ] {
        if padded.contains(&form) {
            return true;
        }
    }
    // A complete-series pack, e.g. "S01-S05".
    if padded.contains("complete series") || padded.contains("complete season") {
        return true;
    }
    if let Some(pos) = padded.find(" s01 s") {
        // "s01 s05" style range — keep when it spans our season.
        let tail = &padded[pos + 6..];
        if let Some(end) = tail.get(..2).and_then(|t| t.parse::<u32>().ok()) {
            if (1..=end).contains(&s) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, hash: &str, seeders: &str, size: &str, imdb: &str) -> RawTorrent {
        RawTorrent {
            name: name.into(),
            info_hash: hash.into(),
            seeders: seeders.into(),
            size: size.into(),
            imdb: imdb.into(),
        }
    }

    const H: &str = "8F50891FCC8AC9B8C0127803F6F30C4F8D611311";

    #[test]
    fn the_no_results_sentinel_is_dropped() {
        let rows = vec![row("No results returned", EMPTY_HASH, "0", "0", "")];
        assert!(parse_rows(rows, "tt1187043", None, None).is_empty());
    }

    #[test]
    fn rows_tagged_with_another_title_are_dropped() {
        let rows = vec![
            row("Right Movie 1080p", H, "10", "1000", "tt1187043"),
            row("Wrong Movie 1080p", H, "99", "1000", "tt0000001"),
        ];
        let out = parse_rows(rows, "tt1187043", None, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Right Movie 1080p");
    }

    #[test]
    fn size_and_seeders_come_through_as_numbers() {
        let rows = vec![row("Movie 1080p", H, "118", "2691958652", "tt1187043")];
        let c = &parse_rows(rows, "tt1187043", None, None)[0];
        assert_eq!(c.seeders, 118);
        assert_eq!(c.size_bytes, Some(2_691_958_652));
    }

    #[test]
    fn dual_audio_is_detected_from_the_name() {
        let rows = vec![row(
            "Movie 2019 1080p BluRay Dual Audio Hindi English x264",
            H,
            "5",
            "1000",
            "tt1187043",
        )];
        let c = &parse_rows(rows, "tt1187043", None, None)[0];
        assert_eq!(c.audio.dual(), crate::audio::Dual::Yes);
    }

    #[test]
    fn episode_releases_and_matching_season_packs_are_kept() {
        assert!(keeps_episode("Breaking Bad S02E01 1080p", Some(2), Some(1)));
        assert!(keeps_episode(
            "Breaking Bad Season 2 Complete 720p",
            Some(2),
            Some(1)
        ));
        assert!(keeps_episode(
            "Breaking Bad S02 Complete 1080p",
            Some(2),
            Some(1)
        ));
    }

    #[test]
    fn a_pack_for_a_different_season_is_rejected() {
        assert!(!keeps_episode(
            "Breaking Bad Season 4 Complete 720p",
            Some(2),
            Some(1)
        ));
        assert!(!keeps_episode(
            "Breaking Bad S05E01 1080p",
            Some(2),
            Some(1)
        ));
    }

    #[test]
    fn complete_series_packs_are_kept() {
        assert!(keeps_episode(
            "Breaking Bad S01-S05 Complete 720p x265",
            Some(2),
            Some(1)
        ));
    }

    #[test]
    fn movies_skip_episode_filtering_entirely() {
        assert!(keeps_episode("3 Idiots 2009 1080p", None, None));
    }
}
