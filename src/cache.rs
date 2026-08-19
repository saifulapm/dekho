//! The piece cache: a size budget, and eviction that knows what you watched.
//!
//! The download dir used to grow forever — a disk leak by design. Now it gets
//! a budget (`cache_max_gb` in the config, `--cache-max-gb` to override,
//! 20 GiB by default) and is pruned at sensible moments: when a playback run
//! starts, and after it ends. Never on a timer — an idle dekho touches nothing.
//!
//! Eviction is least-recently-used, refined by the history store: titles
//! already marked finished go first, titles nobody has heard of next, and
//! anything part-watched last — those pieces are exactly what makes resuming
//! instant. What is being played right now is never touched: every engine
//! mirrors its live torrents' cache entries to a `.dekho-active-<pid>.json`
//! lock file in the download dir, so one dekho's pruner leaves another's
//! stream alone. A lock whose pid is dead is garbage from a killed run and is
//! ignored and removed.
//!
//! "Recently used" is tracked with marker files under `.dekho-lru/` rather
//! than by touching the torrent data itself: a fully cached re-watch downloads
//! nothing, so data mtimes alone would age exactly the entries worth keeping.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::history::History;

/// GiB the piece cache may hold before pruning starts evicting. 0 disables.
pub const DEFAULT_BUDGET_GB: u64 = 20;

/// Entries in the download dir that are not torrent data and are never
/// evicted: the image cache and the api cache live beside the pieces.
const RESERVED: &[&str] = &["img", "api"];

/// Directory of last-used marker files, one per cache entry.
const LRU_DIR: &str = ".dekho-lru";

/// Live-session lock files: `.dekho-active-<pid>.json`.
const ACTIVE_PREFIX: &str = ".dekho-active-";

/// One top-level entry in the download dir: a torrent's directory, or a
/// single-file torrent's file.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// Unix seconds of last use — the LRU marker when one exists, otherwise
    /// the newest mtime found in the data.
    pub last_used: u64,
}

/// What the history store says about a cache entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WatchState {
    /// Watched to the end (or an episode the viewer has moved past). Evict
    /// these first — the pieces have done their job.
    Finished,
    /// Nothing in history matches. Middle priority.
    Unknown,
    /// Matches a title someone is in the middle of. Evict last: these pieces
    /// are what makes resuming instant.
    PartWatched,
}

impl WatchState {
    pub fn label(self) -> &'static str {
        match self {
            WatchState::Finished => "finished",
            WatchState::Unknown => "unknown",
            WatchState::PartWatched => "part-watched",
        }
    }
}

/// The budget in bytes: flag beats config beats default. 0 means unlimited.
pub fn budget_bytes(flag_gb: Option<u64>) -> u64 {
    flag_gb
        .or_else(crate::config::cache_max_gb)
        .unwrap_or(DEFAULT_BUDGET_GB)
        .saturating_mul(1024 * 1024 * 1024)
}

/// The first path component of a relative file path — the cache entry a
/// torrent file lives under. A single-file torrent's entry is the file itself.
pub fn top_component(relative: &str) -> Option<String> {
    let first = relative.split(['/', '\\']).next()?.trim();
    (!first.is_empty()).then(|| first.to_string())
}

/// Everything currently in the cache dir that is torrent data.
pub fn scan(dir: &Path) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || RESERVED.contains(&name.as_str()) {
            continue;
        }
        let path = item.path();
        let (bytes, newest_mtime) = size_and_mtime(&path);
        let marker = dir.join(LRU_DIR).join(&name);
        let last_used = mtime_of(&marker).unwrap_or(newest_mtime);
        entries.push(Entry {
            name,
            path,
            bytes,
            last_used,
        });
    }
    entries
}

/// Recursive size and newest mtime. Torrent trees are shallow, so this stays
/// cheap.
fn size_and_mtime(path: &Path) -> (u64, u64) {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return (0, 0),
    };
    if meta.is_file() {
        return (meta.len(), mtime_of(path).unwrap_or(0));
    }
    if !meta.is_dir() {
        return (0, 0);
    }
    let mut bytes = 0;
    let mut newest = 0;
    if let Ok(read) = std::fs::read_dir(path) {
        for item in read.flatten() {
            let (b, m) = size_and_mtime(&item.path());
            bytes += b;
            newest = newest.max(m);
        }
    }
    if newest == 0 {
        // An empty directory has only its own mtime to go by. A non-empty one
        // must NOT use it: creating the directory is "now" forever, and would
        // mask how old the data inside actually is.
        newest = mtime_of(path).unwrap_or(0);
    }
    (bytes, newest)
}

fn mtime_of(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Mark an entry as used now. Called when a release starts playing.
pub fn touch(dir: &Path, entry_name: &str) {
    let lru = dir.join(LRU_DIR);
    if std::fs::create_dir_all(&lru).is_err() {
        return;
    }
    let marker = lru.join(entry_name);
    // Rewriting refreshes the mtime whether or not the file existed.
    let _ = std::fs::write(marker, b"");
}

/// Mirror the set of cache entries this process is streaming from. Rewritten
/// on every torrent add and forget, so it is always current.
pub fn write_active(dir: &Path, names: &HashSet<String>) {
    let list: Vec<&String> = names.iter().collect();
    let value = json!({ "pid": std::process::id(), "names": list });
    let _ = std::fs::write(active_path(dir), value.to_string());
}

pub fn remove_active(dir: &Path) {
    let _ = std::fs::remove_file(active_path(dir));
}

fn active_path(dir: &Path) -> PathBuf {
    dir.join(format!("{ACTIVE_PREFIX}{}.json", std::process::id()))
}

/// Entries any *live* dekho process is streaming from. Locks left by dead
/// pids are removed on the way through.
pub fn locked_names(dir: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return names;
    };
    for item in read.flatten() {
        let file_name = item.file_name().to_string_lossy().to_string();
        if !file_name.starts_with(ACTIVE_PREFIX) || !file_name.ends_with(".json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(item.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            let _ = std::fs::remove_file(item.path());
            continue;
        };
        let pid = value.get("pid").and_then(Value::as_u64).unwrap_or(0);
        if !pid_alive(pid) {
            let _ = std::fs::remove_file(item.path());
            continue;
        }
        if let Some(list) = value.get("names").and_then(Value::as_array) {
            names.extend(list.iter().filter_map(Value::as_str).map(str::to_string));
        }
    }
    names
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: u64) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn pid_alive(pid: u64) -> bool {
    // No cheap check: err on the side of never evicting a live stream.
    pid > 0
}

/// Lowercased alphanumeric words of a name, for fuzzy matching against
/// history titles.
fn words(s: &str) -> Vec<String> {
    s.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// The `SxxExx` (or `2x05`) an entry name carries, if any.
fn episode_of(name: &str) -> Option<(u32, u32)> {
    let flat = name.to_ascii_lowercase();
    let bytes = flat.as_bytes();
    for (i, _) in flat.match_indices('s') {
        let rest = &bytes[i + 1..];
        let s_digits: usize = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if !(1..=2).contains(&s_digits) || rest.get(s_digits) != Some(&b'e') {
            continue;
        }
        let e_start = s_digits + 1;
        let e_digits = rest[e_start..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if !(1..=3).contains(&e_digits) {
            continue;
        }
        let season: u32 = flat[i + 1..i + 1 + s_digits].parse().ok()?;
        let episode: u32 = flat[i + 1 + e_start..i + 1 + e_start + e_digits]
            .parse()
            .ok()?;
        return Some((season, episode));
    }
    None
}

/// Classify a cache entry against the history store.
///
/// Matching is fuzzy on purpose: a release name is
/// `Fight.Club.1999.1080p.WEB…` and the history entry says `Fight Club`
/// (1999). Every word of the title must appear in the name, and when both
/// sides know a year they must agree. TV entries refine further: an episode
/// the viewer has moved past counts as finished even though the title as a
/// whole is not.
pub fn classify(entry_name: &str, history: &History) -> WatchState {
    let name_words = words(entry_name);
    for entry in &history.items {
        let title_words = words(&entry.title);
        if title_words.is_empty() || !title_words.iter().all(|w| name_words.contains(w)) {
            continue;
        }
        if !entry.year.is_empty() {
            let has_a_year = name_words
                .iter()
                .any(|w| w.len() == 4 && (w.starts_with("19") || w.starts_with("20")));
            if has_a_year && !name_words.contains(&entry.year) {
                continue;
            }
        }
        if entry.kind == "movie" {
            return if entry.finished {
                WatchState::Finished
            } else {
                WatchState::PartWatched
            };
        }
        // TV: a specific episode's pieces are done once the viewer is past it.
        if let (Some((s, e)), Some(es), Some(ee)) = (
            episode_of(entry_name),
            entry.season,
            entry.episode,
        ) {
            let past = s < es || (s == es && e < ee);
            return if entry.finished || past {
                WatchState::Finished
            } else {
                WatchState::PartWatched
            };
        }
        // A season pack, or an entry we cannot pin to an episode: only a
        // finished title frees it.
        return if entry.finished {
            WatchState::Finished
        } else {
            WatchState::PartWatched
        };
    }
    WatchState::Unknown
}

/// Eviction order: finished first, unknown next, part-watched last, LRU within
/// each class. Protected entries are simply absent.
pub fn eviction_order<'a>(
    entries: &'a [Entry],
    history: &History,
    protected: &HashSet<String>,
) -> Vec<&'a Entry> {
    let mut order: Vec<&Entry> = entries
        .iter()
        .filter(|e| !protected.contains(&e.name))
        .collect();
    order.sort_by_key(|e| (classify(&e.name, history), e.last_used));
    order
}

/// Which entries to evict to bring the cache under budget. `entries` is the
/// full scan (protected included — their bytes still count against the
/// budget); `order` is `eviction_order`'s output.
pub fn plan<'a>(entries: &[Entry], order: &[&'a Entry], budget: u64) -> Vec<&'a Entry> {
    let mut used: u64 = entries.iter().map(|e| e.bytes).sum();
    let mut evict = Vec::new();
    for entry in order {
        if used <= budget {
            break;
        }
        used = used.saturating_sub(entry.bytes);
        evict.push(*entry);
    }
    evict
}

/// What a prune did, for reporting.
#[derive(Debug, Default)]
pub struct PruneReport {
    pub evicted: Vec<(String, u64)>,
    pub freed: u64,
    pub used_after: u64,
}

/// Bring the cache under budget. A budget of 0 means unlimited: scan nothing,
/// delete nothing.
///
/// `extra_protected` is what the calling process itself is streaming; the
/// lock files of other processes are honoured on top.
pub fn prune(
    dir: &Path,
    budget: u64,
    history: &History,
    extra_protected: &HashSet<String>,
) -> PruneReport {
    if budget == 0 {
        return PruneReport::default();
    }
    let entries = scan(dir);
    let mut protected = locked_names(dir);
    protected.extend(extra_protected.iter().cloned());

    let order = eviction_order(&entries, history, &protected);
    let evict = plan(&entries, &order, budget);

    let mut report = PruneReport::default();
    for entry in evict {
        let removed = if entry.path.is_dir() {
            std::fs::remove_dir_all(&entry.path).is_ok()
        } else {
            std::fs::remove_file(&entry.path).is_ok()
        };
        if removed {
            let _ = std::fs::remove_file(dir.join(LRU_DIR).join(&entry.name));
            report.freed += entry.bytes;
            report.evicted.push((entry.name.clone(), entry.bytes));
        }
    }
    let used: u64 = entries.iter().map(|e| e.bytes).sum();
    report.used_after = used.saturating_sub(report.freed);
    report
}

/// Remove every entry not held by a live session, whatever the budget.
pub fn clear(dir: &Path) -> PruneReport {
    let entries = scan(dir);
    let protected = locked_names(dir);
    let mut report = PruneReport::default();
    let mut kept: u64 = 0;
    for entry in &entries {
        if protected.contains(&entry.name) {
            kept += entry.bytes;
            continue;
        }
        let removed = if entry.path.is_dir() {
            std::fs::remove_dir_all(&entry.path).is_ok()
        } else {
            std::fs::remove_file(&entry.path).is_ok()
        };
        if removed {
            let _ = std::fs::remove_file(dir.join(LRU_DIR).join(&entry.name));
            report.freed += entry.bytes;
            report.evicted.push((entry.name.clone(), entry.bytes));
        } else {
            kept += entry.bytes;
        }
    }
    report.used_after = kept;
    report
}

/// The cache as JSON, for `dekho api cache` and the human `cache status`.
pub fn status_value(dir: &Path, budget: u64, history: &History) -> Value {
    let mut entries = scan(dir);
    entries.sort_by_key(|e| std::cmp::Reverse(e.last_used));
    let active = locked_names(dir);
    let used: u64 = entries.iter().map(|e| e.bytes).sum();
    let items: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "name": e.name,
                "bytes": e.bytes,
                "last_used": e.last_used,
                "state": classify(&e.name, history).label(),
                "active": active.contains(&e.name),
            })
        })
        .collect();
    json!({
        "dir": dir.display().to_string(),
        "budget_bytes": budget,
        "used_bytes": used,
        "entries": items,
    })
}

/// Unix seconds now, for tests that need a stable clock elsewhere.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{History, Watch};

    fn entry(name: &str, bytes: u64, last_used: u64) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from(name),
            bytes,
            last_used,
        }
    }

    fn movie(id: u32, title: &str, year: &str) -> Watch {
        Watch {
            id,
            kind: "movie",
            title: title.into(),
            year: year.into(),
            poster: String::new(),
            backdrop: String::new(),
            season: None,
            episode: None,
            episode_name: None,
            next: None,
        }
    }

    fn history_with(records: Vec<(Watch, u64, u64)>) -> History {
        let mut h = History::empty();
        for (i, (w, pos, dur)) in records.into_iter().enumerate() {
            h.record(&w, pos, dur, 100 + i as u64);
        }
        h
    }

    #[test]
    fn top_component_takes_the_entry_a_file_lives_under() {
        assert_eq!(
            top_component("Fight.Club.1999/movie.mkv").as_deref(),
            Some("Fight.Club.1999")
        );
        assert_eq!(top_component("movie.mkv").as_deref(), Some("movie.mkv"));
        assert_eq!(top_component(""), None);
    }

    #[test]
    fn a_finished_movie_is_classified_finished() {
        let h = history_with(vec![(movie(550, "Fight Club", "1999"), 9_500, 10_000)]);
        assert_eq!(
            classify("Fight.Club.1999.1080p.WEB-DL.x264", &h),
            WatchState::Finished
        );
    }

    #[test]
    fn a_part_watched_movie_is_protected_longest() {
        let h = history_with(vec![(movie(550, "Fight Club", "1999"), 1_000, 10_000)]);
        assert_eq!(
            classify("Fight.Club.1999.1080p.WEB-DL.x264", &h),
            WatchState::PartWatched
        );
    }

    #[test]
    fn an_unmatched_entry_is_unknown() {
        let h = history_with(vec![(movie(550, "Fight Club", "1999"), 9_500, 10_000)]);
        assert_eq!(
            classify("Some.Other.Movie.2001.720p", &h),
            WatchState::Unknown
        );
    }

    #[test]
    fn a_different_year_is_a_different_film() {
        // A remake must not inherit the original's finished flag.
        let h = history_with(vec![(movie(550, "Suspiria", "1977"), 9_500, 10_000)]);
        assert_eq!(classify("Suspiria.2018.1080p", &h), WatchState::Unknown);
    }

    fn tv(episode: u32, next: Option<(u32, &str)>) -> Watch {
        Watch {
            id: 1396,
            kind: "tv",
            title: "Breaking Bad".into(),
            year: "2008".into(),
            poster: String::new(),
            backdrop: String::new(),
            season: Some(2),
            episode: Some(episode),
            episode_name: None,
            next: next.map(|(n, s)| (n, s.to_string())),
        }
    }

    #[test]
    fn an_episode_the_viewer_moved_past_counts_as_finished() {
        // Watching S02E05 → the S02E04 pieces have done their job.
        let h = history_with(vec![(tv(5, Some((6, "Next"))), 60, 2_760)]);
        assert_eq!(
            classify("Breaking.Bad.S02E04.1080p", &h),
            WatchState::Finished
        );
    }

    #[test]
    fn the_current_and_future_episodes_stay_protected() {
        let h = history_with(vec![(tv(5, Some((6, "Next"))), 60, 2_760)]);
        // The one on screen…
        assert_eq!(
            classify("Breaking.Bad.S02E05.1080p", &h),
            WatchState::PartWatched
        );
        // …and the one prefetched for later.
        assert_eq!(
            classify("Breaking.Bad.S02E06.1080p", &h),
            WatchState::PartWatched
        );
    }

    #[test]
    fn a_season_pack_is_protected_while_the_title_is_in_progress() {
        let h = history_with(vec![(tv(5, Some((6, "Next"))), 60, 2_760)]);
        assert_eq!(
            classify("Breaking.Bad.S02.Complete.1080p", &h),
            WatchState::PartWatched
        );
    }

    #[test]
    fn eviction_prefers_finished_then_unknown_then_part_watched() {
        let h = history_with(vec![
            (movie(1, "Done Film", "2001"), 9_500, 10_000),
            (movie(2, "Half Film", "2002"), 1_000, 10_000),
        ]);
        let entries = vec![
            entry("Half.Film.2002.1080p", 10, 50),
            entry("Mystery.Entry.720p", 10, 10),
            entry("Done.Film.2001.1080p", 10, 90),
        ];
        let order = eviction_order(&entries, &h, &HashSet::new());
        let names: Vec<&str> = order.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Done.Film.2001.1080p",   // finished, despite being newest
                "Mystery.Entry.720p",     // unknown
                "Half.Film.2002.1080p",   // part-watched goes last
            ]
        );
    }

    #[test]
    fn within_a_class_the_least_recently_used_goes_first() {
        let h = History::empty();
        let entries = vec![
            entry("newer", 10, 200),
            entry("older", 10, 100),
        ];
        let order = eviction_order(&entries, &h, &HashSet::new());
        assert_eq!(order[0].name, "older");
    }

    #[test]
    fn protected_entries_never_appear_in_the_order() {
        let h = History::empty();
        let entries = vec![entry("active", 10, 1), entry("idle", 10, 2)];
        let protected: HashSet<String> = ["active".to_string()].into();
        let order = eviction_order(&entries, &h, &protected);
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].name, "idle");
    }

    #[test]
    fn plan_evicts_only_until_the_budget_is_met() {
        let h = History::empty();
        let entries = vec![
            entry("a", 40, 1),
            entry("b", 40, 2),
            entry("c", 40, 3),
        ];
        let order = eviction_order(&entries, &h, &HashSet::new());
        // 120 used, budget 80: one eviction is enough.
        let evict = plan(&entries, &order, 80);
        assert_eq!(evict.len(), 1);
        assert_eq!(evict[0].name, "a");
    }

    #[test]
    fn a_cache_under_budget_evicts_nothing() {
        let entries = vec![entry("a", 40, 1)];
        let order: Vec<&Entry> = entries.iter().collect();
        assert!(plan(&entries, &order, 100).is_empty());
    }

    #[test]
    fn protected_bytes_still_count_against_the_budget() {
        let h = History::empty();
        let entries = vec![entry("active", 90, 1), entry("idle", 20, 2)];
        let protected: HashSet<String> = ["active".to_string()].into();
        let order = eviction_order(&entries, &h, &protected);
        // 110 used, budget 100: the idle entry must go even though the active
        // one is the elephant.
        let evict = plan(&entries, &order, 100);
        assert_eq!(evict.len(), 1);
        assert_eq!(evict[0].name, "idle");
    }

    #[test]
    fn episode_markers_parse_from_release_names() {
        assert_eq!(episode_of("Show.S02E05.1080p"), Some((2, 5)));
        assert_eq!(episode_of("show s2e5 x265"), Some((2, 5)));
        assert_eq!(episode_of("Show.Season.2.Complete"), None);
        assert_eq!(episode_of("Movie.2019.1080p"), None);
    }

    #[test]
    fn prune_on_a_real_directory_respects_protection_and_budget() {
        let dir = std::env::temp_dir().join(format!("dekho-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Three 1 MB entries; give them distinct LRU ages.
        for name in ["one", "two", "three"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("video.mkv"), vec![0u8; 1_000_000]).unwrap();
        }
        touch(&dir, "one");
        touch(&dir, "two");
        touch(&dir, "three");
        // Make "one" oldest and "three" newest by rewriting the markers with
        // explicit mtimes via the data files instead — simplest portable way:
        // order by marker rewrite sequence is not reliable within a second, so
        // set the data mtimes and drop the markers.
        let _ = std::fs::remove_dir_all(dir.join(LRU_DIR));
        for (name, age) in [("one", 3_000u64), ("two", 2_000), ("three", 1_000)] {
            let f = std::fs::File::options()
                .write(true)
                .open(dir.join(name).join("video.mkv"))
                .unwrap();
            let t = SystemTime::now() - std::time::Duration::from_secs(age);
            f.set_modified(t).unwrap();
        }
        // Protect "one" (the LRU victim) as if it were playing.
        let protected: HashSet<String> = ["one".to_string()].into();
        // Budget of 1.5 MB: two entries must go, and they must be the
        // unprotected ones.
        let report = prune(&dir, 1_500_000, &History::empty(), &protected);
        let evicted: Vec<&str> = report.evicted.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(evicted, vec!["two", "three"]);
        assert!(dir.join("one").exists(), "the active entry survives");
        assert!(!dir.join("two").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zero_budget_disables_pruning() {
        let report = prune(
            Path::new("/nonexistent"),
            0,
            &History::empty(),
            &HashSet::new(),
        );
        assert!(report.evicted.is_empty());
    }
}
