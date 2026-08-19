//! Which releases to try, and in what order.
//!
//! "Highest quality" and "plays without buffering" pull against each other, and
//! this is where they get reconciled. The ordering is quality-first, but three
//! filters run before it:
//!
//! - **A quality ceiling** (`--quality`), so nothing above what was asked for is
//!   considered at all.
//! - **A bitrate ceiling** (`--max-bitrate`). A 4K WEB-DL at 20 Mbps streams
//!   beautifully; a 4K Blu-ray remux of the same film wants 80 Mbps sustained
//!   and no swarm delivers that reliably. Both are "4K", so quality alone
//!   cannot tell them apart — bitrate can.
//! - **A seeder floor** (`--min-seeders`), which drops the releases that would
//!   only ever fail the probe, so we do not spend 30 seconds proving it.
//!
//! Whatever survives is still only a *candidate*: `engine::probe` measures the
//! swarm before any of it is played.

use crate::torrentio::{Candidate, Quality};

/// How many releases are worth probing before we settle. Each failed probe
/// costs real seconds, and past the first few the quality is dropping anyway.
pub const MAX_ATTEMPTS: usize = 4;

pub struct Filters {
    pub max_quality: Quality,
    /// Bits per second. Releases needing more than this are skipped.
    pub max_bitrate: u64,
    pub min_seeders: u32,
}

/// Order candidates best-first, dropping the ones not worth probing.
///
/// `runtime_secs` converts a release's size into the bitrate it needs; without a
/// size we cannot compute that, so such releases are kept (the probe judges
/// them) but sorted below ones we can reason about.
pub fn shortlist(
    candidates: Vec<Candidate>,
    filters: &Filters,
    runtime_secs: u32,
) -> Vec<Candidate> {
    let mut kept: Vec<Candidate> = candidates
        .into_iter()
        .filter(|c| c.quality <= filters.max_quality)
        .filter(|c| c.seeders >= filters.min_seeders)
        .filter(|c| match c.required_bps(runtime_secs) {
            Some(bps) => bps <= filters.max_bitrate,
            // Unknown size: let the probe decide rather than guessing.
            None => true,
        })
        .collect();

    kept.sort_by(|a, b| {
        b.quality
            .cmp(&a.quality)
            // A known size is worth more than an unknown one at equal quality:
            // we can size the buffer for it and predict whether it will hold.
            .then_with(|| b.size_bytes.is_some().cmp(&a.size_bytes.is_some()))
            .then_with(|| b.seeders.cmp(&a.seeders))
    });
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(quality: Quality, size_gb: Option<f64>, seeders: u32) -> Candidate {
        Candidate {
            quality,
            title: format!("{} {}s", quality.label(), seeders),
            size_bytes: size_gb.map(|g| (g * 1024.0 * 1024.0 * 1024.0) as u64),
            seeders,
            file_idx: None,
            magnet: String::new(),
        }
    }

    fn filters() -> Filters {
        Filters {
            max_quality: Quality::P2160,
            max_bitrate: 40_000_000,
            min_seeders: 4,
        }
    }

    // Two hours.
    const RUNTIME: u32 = 7200;

    #[test]
    fn a_4k_remux_is_dropped_but_a_4k_web_dl_survives() {
        // 74 GB over 2h ≈ 88 Mbps — over the ceiling.
        let remux = candidate(Quality::P2160, Some(74.0), 40);
        // 18 GB over 2h ≈ 21 Mbps — fine.
        let webdl = candidate(Quality::P2160, Some(18.0), 40);
        let out = shortlist(vec![remux, webdl], &filters(), RUNTIME);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].size_bytes, Some((18.0 * 1024.0f64.powi(3)) as u64));
    }

    #[test]
    fn quality_ceiling_excludes_everything_above_it() {
        let f = Filters {
            max_quality: Quality::P1080,
            ..filters()
        };
        let out = shortlist(
            vec![
                candidate(Quality::P2160, Some(18.0), 500),
                candidate(Quality::P1080, Some(8.0), 10),
            ],
            &f,
            RUNTIME,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].quality, Quality::P1080);
    }

    #[test]
    fn seeder_floor_drops_hopeless_swarms() {
        let out = shortlist(
            vec![
                candidate(Quality::P1080, Some(8.0), 1),
                candidate(Quality::P720, Some(4.0), 100),
            ],
            &filters(),
            RUNTIME,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].quality, Quality::P720);
    }

    #[test]
    fn higher_quality_outranks_more_seeders() {
        let out = shortlist(
            vec![
                candidate(Quality::P720, Some(4.0), 900),
                candidate(Quality::P1080, Some(8.0), 20),
            ],
            &filters(),
            RUNTIME,
        );
        assert_eq!(out[0].quality, Quality::P1080);
    }

    #[test]
    fn seeders_break_ties_within_a_quality() {
        let out = shortlist(
            vec![
                candidate(Quality::P1080, Some(8.0), 20),
                candidate(Quality::P1080, Some(8.0), 300),
            ],
            &filters(),
            RUNTIME,
        );
        assert_eq!(out[0].seeders, 300);
    }

    #[test]
    fn unknown_size_is_kept_but_ranked_below_a_known_one() {
        let out = shortlist(
            vec![
                candidate(Quality::P1080, None, 900),
                candidate(Quality::P1080, Some(8.0), 20),
            ],
            &filters(),
            RUNTIME,
        );
        assert_eq!(out.len(), 2);
        assert!(out[0].size_bytes.is_some());
    }

    #[test]
    fn everything_filtered_out_yields_an_empty_shortlist() {
        let out = shortlist(
            vec![candidate(Quality::P1080, Some(8.0), 0)],
            &filters(),
            RUNTIME,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn a_short_runtime_raises_the_computed_bitrate() {
        // 8 GB over 22 minutes is ~48 Mbps — too hot, even though 8 GB over two
        // hours would have been fine. Episode runtimes matter here.
        let out = shortlist(
            vec![candidate(Quality::P1080, Some(8.0), 50)],
            &filters(),
            22 * 60,
        );
        assert!(out.is_empty());
    }
}
