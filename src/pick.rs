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

use std::collections::HashSet;

use crate::audio::Dual;
use crate::torrentio::{Candidate, Quality};

/// How many releases are worth probing before we settle. Each failed probe
/// costs real seconds, and past the first few the quality is dropping anyway.
pub const MAX_ATTEMPTS: usize = 5;

/// How many releases from the same quality tier may be probed before dropping
/// to the next tier down.
///
/// Without this, a quality-sorted list spends the entire attempt budget on 4K:
/// observed against Fight Club, where four 4K releases (33 GB, 6.8 GB, 38 GB,
/// 7.9 GB) were tried in a row and a perfectly streamable 1080p was never
/// reached. If two releases in a tier both fail, the tier is the problem, not
/// the release.
const MAX_PER_TIER: usize = 2;

pub struct Filters {
    pub max_quality: Quality,
    /// Bits per second. Releases needing more than this are skipped.
    pub max_bitrate: u64,
    pub min_seeders: u32,
    /// How to treat Hindi+English dual-audio releases.
    pub dual: DualPreference,
    /// The remembered link estimate, when trusted. Below `link::SLOW_LINK_BPS`
    /// the ordering starts caring about swarm health more than quality.
    pub link_bps: Option<u64>,
}

/// Seeders at and above which a swarm counts as healthy. On a thin pipe any
/// healthy swarm delivers the whole link, so among healthy swarms quality can
/// decide as usual — the mistake this prevents is spending probe budgets on a
/// high-quality release with a handful of seeders when a well-seeded one a
/// tier down would have saturated the line.
const HEALTHY_SEEDERS: u32 = 30;

/// What to do about dual audio.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DualPreference {
    /// Ignore audio entirely; rank on quality as usual.
    #[default]
    Ignore,
    /// Rank dual-audio releases above everything else, but keep the rest as a
    /// fallback for titles that simply have no dual-audio release.
    Prefer,
    /// Drop anything that is not dual audio.
    Only,
}

impl DualPreference {
    /// The config file's word for it. `None` is the caller's cue to explain
    /// what the accepted words are.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "ignore" | "no" | "false" => Some(Self::Ignore),
            "prefer" | "on" | "yes" | "true" => Some(Self::Prefer),
            "only" => Some(Self::Only),
            _ => None,
        }
    }
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
        .filter(|c| match filters.dual {
            // `Likely` counts: an Indian release saying "Dual Audio" and naming
            // only Hindi almost always carries English too, and excluding those
            // would throw away most of what the flag is for.
            DualPreference::Only => c.audio.dual() > Dual::No,
            _ => true,
        })
        .filter(|c| match c.required_bps(runtime_secs) {
            Some(bps) => bps <= filters.max_bitrate,
            // Unknown size: assume the tier's typical bitrate. On a slow link
            // an unknown-size 4K would otherwise sail past the ceiling and
            // burn a probe budget the known-size releases were filtered to
            // protect.
            None => c.quality.assumed_bps() <= filters.max_bitrate,
        })
        .collect();

    // On a slow link, swarm health outranks quality: an under-seeded release a
    // tier up cannot even saturate a thin pipe, while any healthy swarm can.
    let slow_link = filters
        .link_bps
        .map(|bps| bps < crate::link::SLOW_LINK_BPS)
        .unwrap_or(false);

    kept.sort_by(|a, b| {
        // Dual audio outranks quality when asked for: a 1080p Hindi+English is
        // the point, and a 4K English-only is not a better answer to it.
        dual_rank(b, filters)
            .cmp(&dual_rank(a, filters))
            .then_with(|| {
                if slow_link {
                    (b.seeders >= HEALTHY_SEEDERS).cmp(&(a.seeders >= HEALTHY_SEEDERS))
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| b.quality.cmp(&a.quality))
            // A known size is worth more than an unknown one at equal quality:
            // we can size the buffer for it and predict whether it will hold.
            .then_with(|| b.size_bytes.is_some().cmp(&a.size_bytes.is_some()))
            .then_with(|| b.seeders.cmp(&a.seeders))
    });
    kept
}

/// What `title_guard` decided.
pub struct Guarded {
    pub kept: Vec<Candidate>,
    /// Releases dropped for being named like a different title.
    pub dropped: usize,
    /// True when no release matched at all, so the guard stood aside.
    pub fail_open: bool,
}

/// Drop releases named like a different title entirely.
///
/// Indexers occasionally file a stranger under a title's IMDB id — observed:
/// Torrentio serving "La casa di Davide" as a Money Heist episode, whose fake
/// 2160p then outranked every genuine 1080p release and auto-played. A release
/// matches when every significant word of at least one accepted title appears
/// in its name; articles and connectives are not significant, because release
/// names drop and translate them freely. Callers pass both the display title
/// and the original-language one — releases are named after either.
///
/// Deliberately fail-open twice over: a title with no significant words cannot
/// judge anything, and if nothing at all matches the guard stands aside — one
/// naming style we cannot read is far more likely than every torrent being for
/// the wrong show.
pub fn title_guard(candidates: Vec<Candidate>, titles: &[&str]) -> Guarded {
    let keys: Vec<Vec<String>> = titles
        .iter()
        .map(|t| significant_words(t))
        .filter(|w| !w.is_empty())
        .collect();
    if keys.is_empty() {
        return Guarded {
            kept: candidates,
            dropped: 0,
            fail_open: false,
        };
    }

    let (kept, dropped): (Vec<Candidate>, Vec<Candidate>) =
        candidates.into_iter().partition(|c| {
            let name: HashSet<String> = words_of(&c.title).into_iter().collect();
            keys.iter().any(|k| k.iter().all(|w| name.contains(w)))
        });
    if kept.is_empty() {
        return Guarded {
            kept: dropped,
            dropped: 0,
            fail_open: true,
        };
    }
    Guarded {
        dropped: dropped.len(),
        kept,
        fail_open: false,
    }
}

/// Words release names freely drop, translate, or reorder around — never
/// enough on their own to identify a title.
const TITLE_STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "of", "or", "in", "on", "at", "to", // English
    "la", "le", "les", "el", "los", "las", "un", "une", "uno", "una", "y", "o", // French/Spanish
    "de", "di", "da", "del", "della", "du", "des", "il", "lo", "e", "et", // more Romance
    "der", "die", "das", "den", "ein", "eine", "und", // German
    "um", "uma", "do", "dos", "as", "os", // Portuguese
];

/// The words of a title that a matching release name must carry.
fn significant_words(title: &str) -> Vec<String> {
    words_of(title)
        .into_iter()
        .filter(|w| !TITLE_STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Lowercased, accent-folded, alphanumeric words of a string, so
/// `La.Casa.De.Papel` and `la casa de papel` come out identical.
fn words_of(s: &str) -> Vec<String> {
    let mut folded = String::with_capacity(s.len());
    for c in s.to_lowercase().chars() {
        fold_push(&mut folded, c);
    }
    folded.split_whitespace().map(str::to_string).collect()
}

/// ASCII-fold the Latin accents release groups actually vary on. Anything
/// else alphanumeric passes through; separators become spaces. Apostrophes
/// vanish rather than split, so `Don't` and `Dont` agree.
fn fold_push(out: &mut String, c: char) {
    match c {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' => out.push('a'),
        'ç' | 'ć' | 'č' => out.push('c'),
        'é' | 'è' | 'ê' | 'ë' | 'ē' => out.push('e'),
        'í' | 'ì' | 'î' | 'ï' => out.push('i'),
        'ñ' | 'ń' => out.push('n'),
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' => out.push('o'),
        'ú' | 'ù' | 'û' | 'ü' => out.push('u'),
        'ý' | 'ÿ' => out.push('y'),
        'ß' => out.push_str("ss"),
        'æ' => out.push_str("ae"),
        'œ' => out.push_str("oe"),
        '\'' | '’' => {}
        c if c.is_alphanumeric() => out.push(c),
        _ => out.push(' '),
    }
}

/// Sort key for the dual-audio preference; constant when it is off.
fn dual_rank(c: &Candidate, filters: &Filters) -> u8 {
    dual_rank_of(c, filters.dual)
}

/// How well a candidate answers the dual-audio preference. Public so replay's
/// promotion can refuse to move a cached English-only winner above the dual
/// release the ordering just asked for.
pub fn dual_rank_of(c: &Candidate, dual: DualPreference) -> u8 {
    match dual {
        DualPreference::Ignore => 0,
        _ => match c.audio.dual() {
            Dual::Yes => 2,
            Dual::Likely => 1,
            Dual::No => 0,
        },
    }
}

/// The order releases are actually probed in.
///
/// Takes the shortlist and caps how many come from any one quality tier, so the
/// attempt budget descends through tiers instead of being spent entirely on the
/// top one. Anything left over is appended, so a list that is all one tier still
/// gets its full budget rather than being truncated to `MAX_PER_TIER`.
pub fn attempt_order(shortlist: &[Candidate], max_attempts: usize) -> Vec<&Candidate> {
    let mut per_tier: Vec<(Quality, usize)> = Vec::new();
    let mut primary = Vec::new();
    let mut overflow = Vec::new();

    for c in shortlist {
        let seen = match per_tier.iter_mut().find(|(q, _)| *q == c.quality) {
            Some((_, n)) => n,
            None => {
                per_tier.push((c.quality, 0));
                &mut per_tier.last_mut().unwrap().1
            }
        };
        if *seen < MAX_PER_TIER {
            *seen += 1;
            primary.push(c);
        } else {
            overflow.push(c);
        }
    }

    primary.extend(overflow);
    primary.truncate(max_attempts);
    primary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(quality: Quality, size_gb: Option<f64>, seeders: u32) -> Candidate {
        named(quality, size_gb, seeders, "")
    }

    /// A candidate whose release name carries `extra`, so audio detection sees it.
    fn named(quality: Quality, size_gb: Option<f64>, seeders: u32, extra: &str) -> Candidate {
        let title = format!("{} {}s {extra}", quality.label(), seeders);
        Candidate {
            quality,
            size_bytes: size_gb.map(|g| (g * 1024.0 * 1024.0 * 1024.0) as u64),
            seeders,
            file_idx: None,
            magnet: String::new(),
            audio: crate::audio::detect(&title),
            title,
        }
    }

    fn filters() -> Filters {
        Filters {
            max_quality: Quality::P2160,
            max_bitrate: 40_000_000,
            min_seeders: 4,
            dual: DualPreference::Ignore,
            link_bps: None,
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
    fn attempts_descend_through_tiers_instead_of_exhausting_the_top_one() {
        // The Fight Club case: four 4K releases ahead of any 1080p.
        let list = vec![
            candidate(Quality::P2160, Some(33.0), 40),
            candidate(Quality::P2160, Some(6.8), 35),
            candidate(Quality::P2160, Some(38.0), 32),
            candidate(Quality::P2160, Some(7.9), 16),
            candidate(Quality::P1080, Some(8.0), 500),
            candidate(Quality::P720, Some(4.0), 900),
        ];
        let order = attempt_order(&list, MAX_ATTEMPTS);
        let tiers: Vec<Quality> = order.iter().map(|c| c.quality).collect();
        assert_eq!(
            &tiers[..3],
            &[Quality::P2160, Quality::P2160, Quality::P1080],
            "must reach 1080p by the third attempt"
        );
        assert!(tiers.contains(&Quality::P720), "and 720p within the budget");
    }

    #[test]
    fn a_single_tier_still_gets_the_full_attempt_budget() {
        let list: Vec<Candidate> = (0..6)
            .map(|i| candidate(Quality::P1080, Some(8.0), 100 - i))
            .collect();
        assert_eq!(attempt_order(&list, MAX_ATTEMPTS).len(), MAX_ATTEMPTS);
    }

    #[test]
    fn attempt_order_preserves_rank_within_a_tier() {
        let list = vec![
            candidate(Quality::P1080, Some(8.0), 900),
            candidate(Quality::P1080, Some(8.0), 500),
            candidate(Quality::P1080, Some(8.0), 100),
        ];
        let order = attempt_order(&list, MAX_ATTEMPTS);
        assert_eq!(order[0].seeders, 900);
        assert_eq!(order[1].seeders, 500);
    }

    #[test]
    fn attempt_order_on_an_empty_shortlist_is_empty() {
        assert!(attempt_order(&[], MAX_ATTEMPTS).is_empty());
    }

    #[test]
    fn prefer_puts_dual_audio_above_higher_quality() {
        let f = Filters {
            dual: DualPreference::Prefer,
            ..filters()
        };
        let out = shortlist(
            vec![
                named(Quality::P2160, Some(18.0), 900, "English"),
                named(Quality::P1080, Some(8.0), 20, "Dual Audio Hindi English"),
            ],
            &f,
            RUNTIME,
        );
        assert_eq!(
            out[0].quality,
            Quality::P1080,
            "a 4K English-only is not a better answer to 'dual audio'"
        );
    }

    #[test]
    fn prefer_still_keeps_non_dual_as_a_fallback() {
        let f = Filters {
            dual: DualPreference::Prefer,
            ..filters()
        };
        let out = shortlist(
            vec![named(Quality::P1080, Some(8.0), 20, "English only")],
            &f,
            RUNTIME,
        );
        assert_eq!(out.len(), 1, "nothing dual exists; play something anyway");
    }

    #[test]
    fn only_drops_everything_that_is_not_dual() {
        let f = Filters {
            dual: DualPreference::Only,
            ..filters()
        };
        let out = shortlist(
            vec![
                named(Quality::P2160, Some(18.0), 900, "English"),
                named(Quality::P1080, Some(8.0), 20, "Dual Audio Hindi English"),
            ],
            &f,
            RUNTIME,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].audio.dual(), Dual::Yes);
    }

    #[test]
    fn only_accepts_a_likely_dual_release() {
        // "Dual Audio" naming just Hindi is the common Indian release form.
        let f = Filters {
            dual: DualPreference::Only,
            ..filters()
        };
        let out = shortlist(
            vec![named(
                Quality::P1080,
                Some(8.0),
                20,
                "Dual Audio Hindi DD5 1",
            )],
            &f,
            RUNTIME,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].audio.dual(), Dual::Likely);
    }

    #[test]
    fn definite_dual_outranks_likely_dual() {
        let f = Filters {
            dual: DualPreference::Prefer,
            ..filters()
        };
        let out = shortlist(
            vec![
                named(Quality::P1080, Some(8.0), 900, "Dual Audio Hindi DD5 1"),
                named(Quality::P1080, Some(8.0), 10, "Dual Audio Hindi English"),
            ],
            &f,
            RUNTIME,
        );
        assert_eq!(out[0].audio.dual(), Dual::Yes);
    }

    #[test]
    fn ignore_leaves_the_quality_ordering_untouched() {
        let out = shortlist(
            vec![
                named(Quality::P1080, Some(8.0), 20, "Dual Audio Hindi English"),
                named(Quality::P2160, Some(18.0), 20, "English"),
            ],
            &filters(),
            RUNTIME,
        );
        assert_eq!(out[0].quality, Quality::P2160);
    }

    #[test]
    fn a_slow_link_ranks_a_healthy_swarm_above_a_thin_higher_quality_one() {
        let f = Filters {
            link_bps: Some(6_000_000),
            max_bitrate: 5_000_000,
            ..filters()
        };
        let out = shortlist(
            vec![
                // 1080p that fits the ceiling but has 5 seeders.
                candidate(Quality::P1080, Some(4.0), 5),
                // 720p with a swarm that will actually saturate the line.
                candidate(Quality::P720, Some(2.0), 400),
            ],
            &f,
            RUNTIME,
        );
        assert_eq!(
            out[0].quality,
            Quality::P720,
            "on a thin pipe swarm health beats a quality tier"
        );
    }

    #[test]
    fn a_slow_link_still_prefers_quality_between_healthy_swarms() {
        let f = Filters {
            link_bps: Some(6_000_000),
            max_bitrate: 5_000_000,
            ..filters()
        };
        let out = shortlist(
            vec![
                candidate(Quality::P720, Some(2.0), 900),
                candidate(Quality::P1080, Some(4.0), 60),
            ],
            &f,
            RUNTIME,
        );
        assert_eq!(
            out[0].quality,
            Quality::P1080,
            "both swarms can fill the link, so quality decides"
        );
    }

    #[test]
    fn a_fast_link_keeps_the_quality_first_ordering() {
        let f = Filters {
            link_bps: Some(80_000_000),
            ..filters()
        };
        let out = shortlist(
            vec![
                candidate(Quality::P720, Some(4.0), 900),
                candidate(Quality::P1080, Some(8.0), 5),
            ],
            &f,
            RUNTIME,
        );
        assert_eq!(out[0].quality, Quality::P1080);
    }

    #[test]
    fn an_unknown_size_release_is_judged_by_its_tiers_typical_bitrate() {
        // 5 Mbps ceiling: an unknown-size 4K (assumed ~16 Mbps) is not worth a
        // probe budget, an unknown-size 720p (assumed ~4 Mbps) is.
        let f = Filters {
            max_bitrate: 5_000_000,
            ..filters()
        };
        let out = shortlist(
            vec![
                candidate(Quality::P2160, None, 500),
                candidate(Quality::P720, None, 500),
            ],
            &f,
            RUNTIME,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].quality, Quality::P720);
    }

    /// A candidate whose release name is `title`, for the title guard.
    fn release(title: &str) -> Candidate {
        let mut c = candidate(Quality::P1080, Some(4.0), 50);
        c.title = title.to_string();
        c
    }

    #[test]
    fn the_config_words_for_dual_parse() {
        assert_eq!(DualPreference::parse("prefer"), Some(DualPreference::Prefer));
        assert_eq!(DualPreference::parse(" Only "), Some(DualPreference::Only));
        assert_eq!(DualPreference::parse("off"), Some(DualPreference::Ignore));
        assert_eq!(DualPreference::parse("both"), None);
    }

    #[test]
    fn a_stranger_under_the_right_imdb_id_is_dropped() {
        // Observed live: Torrentio served this for Money Heist S01E01, and its
        // fake 4K outranked every genuine release.
        let guarded = title_guard(
            vec![
                release("La.casa.di.Davide.S01E01-03.2160p.AMZN.WEB-DL.ITA-ENG.DDP5.1"),
                release("Money.Heist.S01E01.1080p.NF.WEB-DL.DDP5.1.x264"),
            ],
            &["Money Heist", "La casa de papel"],
        );
        assert_eq!(guarded.kept.len(), 1);
        assert_eq!(guarded.dropped, 1);
        assert!(!guarded.fail_open);
        assert!(guarded.kept[0].title.starts_with("Money.Heist"));
    }

    #[test]
    fn the_original_language_name_matches_too() {
        let guarded = title_guard(
            vec![release("La.Casa.De.Papel.S01.COMPLETE.1080p.WEB-DL")],
            &["Money Heist", "La casa de papel"],
        );
        assert_eq!(guarded.kept.len(), 1);
        assert_eq!(guarded.dropped, 0);
    }

    #[test]
    fn articles_and_accents_do_not_defeat_a_match() {
        // "La" dropped by the release group, é folded to e.
        let guarded = title_guard(
            vec![release("Casa.de.Papel.S02.SPANISH.720p")],
            &["La casa de papél"],
        );
        assert_eq!(guarded.dropped, 0);
        // Apostrophes vanish rather than split.
        let guarded = title_guard(vec![release("Dont.Look.Up.2021.1080p")], &["Don't Look Up"]);
        assert_eq!(guarded.dropped, 0);
    }

    #[test]
    fn nothing_matching_fails_open() {
        // Blind spot, not 45 wrong torrents: keep everything, say so.
        let guarded = title_guard(
            vec![release("LCDP.S01.1080p"), release("LCDP.S02.1080p")],
            &["La casa de papel"],
        );
        assert_eq!(guarded.kept.len(), 2);
        assert_eq!(guarded.dropped, 0);
        assert!(guarded.fail_open);
    }

    #[test]
    fn an_unjudgeable_title_leaves_the_list_alone() {
        // Every word is a stopword; empty original title comes in as "".
        let guarded = title_guard(vec![release("Anything.At.All.1080p")], &["The", ""]);
        assert_eq!(guarded.kept.len(), 1);
        assert_eq!(guarded.dropped, 0);
        assert!(!guarded.fail_open);
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
