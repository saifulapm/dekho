//! Watch history: what you were in the middle of, and where you were in it.
//!
//! One entry per *title*, not per episode. The question anything reading this
//! asks is "what do I continue?", and the answer is "Breaking Bad, S2E5" — not
//! a list of every episode ever started. So finishing an episode rewrites the
//! entry to point at the next one, and "Continue" always names something worth
//! clicking.
//!
//! Writing is atomic (temp file, then rename) and best-effort. A state file is
//! not worth interrupting playback for: an IO error warns once and is dropped,
//! and a half-written file from a killed run reads back as no history at all
//! rather than as a failure.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Bumped only if the shape changes in a way an older dekho cannot read.
const VERSION: u32 = 1;

/// Entries kept, dropping the least recently updated. A rail shows twenty; the
/// rest is history nobody scrolls to, and the whole file is rewritten on every
/// save.
const MAX_ENTRIES: usize = 100;

/// Fraction watched at which a title counts as finished. Credits, and the
/// "skip to the next episode" card, live in the last tenth.
const FINISHED_FRACTION: f64 = 0.90;

/// Never resume inside this much of the end. Reopening a title 30 seconds
/// before the credits is never what anyone meant by "continue".
const RESUME_TAIL_SECS: u64 = 60;

/// Floor on how often playback progress reaches the disk. mpv volunteers a
/// position roughly every second and none of them are worth a write.
const WRITE_EVERY: Duration = Duration::from_secs(10);

/// Where the history file lives.
pub fn path() -> PathBuf {
    crate::xdg::state_home().join("dekho").join("history.json")
}

/// Unix seconds, for `updated`.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One title you have watched some of.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: u32,
    /// `movie` or `tv`.
    pub kind: String,
    pub title: String,
    pub year: String,
    pub poster: String,
    pub backdrop: String,
    /// All three are null for a movie.
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub episode_name: Option<String>,
    /// Seconds into the title, and its length. Duration is 0 when mpv never
    /// reported one.
    pub position: u64,
    pub duration: u64,
    pub finished: bool,
    /// Unix seconds.
    pub updated: u64,
}

impl Entry {
    /// How far through, 0.0 when the duration is unknown.
    pub fn progress(&self) -> f64 {
        if self.duration == 0 {
            return 0.0;
        }
        // mpv occasionally reports a position past a container's declared
        // duration, which would otherwise read as 103% watched.
        (self.position as f64 / self.duration as f64).clamp(0.0, 1.0)
    }

    /// Where `--resume` should start this, if anywhere.
    pub fn resume_position(&self) -> Option<u64> {
        if self.position == 0 {
            return None;
        }
        if self.duration > 0 && self.position + RESUME_TAIL_SECS >= self.duration {
            return None;
        }
        Some(self.position)
    }

    /// The entry as a caller sees it, progress included.
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "title": self.title,
            "year": self.year,
            "poster": self.poster,
            "backdrop": self.backdrop,
            "season": self.season,
            "episode": self.episode,
            "episode_name": self.episode_name,
            "position": self.position,
            "duration": self.duration,
            "finished": self.finished,
            "updated": self.updated,
            "progress": self.progress(),
        })
    }
}

/// What is on screen right now, as the store needs it described.
#[derive(Clone, Debug)]
pub struct Watch {
    pub id: u32,
    pub kind: &'static str,
    pub title: String,
    pub year: String,
    pub poster: String,
    pub backdrop: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub episode_name: Option<String>,
    /// The episode after this one in the same season, so finishing can advance
    /// to it. `None` for a movie and for the last episode of a season.
    pub next: Option<(u32, String)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct History {
    pub version: u32,
    #[serde(default)]
    pub items: Vec<Entry>,
}

impl History {
    pub fn empty() -> Self {
        Self {
            version: VERSION,
            items: Vec::new(),
        }
    }

    /// Read the store. Anything unreadable is treated as empty — the next save
    /// replaces it, which beats refusing to play because a file got truncated.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(Self::empty)
    }

    /// Write the store, atomically.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .context("the history path has no parent directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        // A rename is atomic on the same filesystem, so a crash mid-write
        // leaves the previous history intact rather than a truncated file. The
        // pid keeps two dekho processes from sharing one temp file.
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn entry(&self, id: u32, kind: &str) -> Option<&Entry> {
        self.items.iter().find(|e| e.id == id && e.kind == kind)
    }

    /// Newest first, capped. Sorted here rather than trusted from the file, so
    /// a store written by another process still comes back in order.
    pub fn recent(&self, limit: usize) -> Vec<&Entry> {
        let mut items: Vec<&Entry> = self.items.iter().collect();
        items.sort_by_key(|e| std::cmp::Reverse(e.updated));
        items.truncate(limit);
        items
    }

    /// Record where playback has reached, applying the finished rules.
    pub fn record(&mut self, watch: &Watch, position: u64, duration: u64, now: u64) {
        let finished = duration > 0 && position as f64 >= duration as f64 * FINISHED_FRACTION;

        let mut entry = Entry {
            id: watch.id,
            kind: watch.kind.to_string(),
            title: watch.title.clone(),
            year: watch.year.clone(),
            poster: watch.poster.clone(),
            backdrop: watch.backdrop.clone(),
            season: watch.season,
            episode: watch.episode,
            episode_name: watch.episode_name.clone(),
            position,
            duration,
            finished,
            updated: now,
        };

        // A finished episode becomes the next one, so "Continue" points at what
        // you would actually watch rather than at something you just watched.
        // The last episode of a season has nowhere to go and stays finished.
        if finished {
            if let Some((number, name)) = &watch.next {
                entry.episode = Some(*number);
                entry.episode_name = Some(name.clone());
                entry.position = 0;
                // Unknown until mpv opens it.
                entry.duration = 0;
                entry.finished = false;
            }
        }

        self.items
            .retain(|e| !(e.id == entry.id && e.kind == entry.kind));
        self.items.insert(0, entry);
        // Stable, so the entry just inserted stays ahead of anything sharing
        // its timestamp.
        self.items.sort_by_key(|e| std::cmp::Reverse(e.updated));
        self.items.truncate(MAX_ENTRIES);
    }
}

/// Keeps the store in step with what mpv is doing.
///
/// Shared with the mpv IPC reader, which calls in from its own task whenever a
/// position arrives — hence the mutex and the throttle. Nothing here can fail
/// loudly: playback continues whatever the filesystem does.
pub struct Recorder {
    path: PathBuf,
    state: Mutex<State>,
}

struct State {
    history: History,
    current: Option<Watch>,
    position: u64,
    duration: u64,
    /// Set once mpv has reported a real position, so quitting before playback
    /// starts does not invent an entry.
    seen: bool,
    last_write: Option<Instant>,
    /// One warning per run: a failing write fails every ten seconds otherwise,
    /// and the second line adds nothing.
    warned: bool,
}

impl Recorder {
    pub fn new(path: PathBuf) -> Arc<Self> {
        let history = History::load(&path);
        Arc::new(Self {
            path,
            state: Mutex::new(State {
                history,
                current: None,
                position: 0,
                duration: 0,
                seen: false,
                last_write: None,
                warned: false,
            }),
        })
    }

    /// What is playing now. Recording starts here and stops at the next flush.
    pub fn set_current(&self, watch: Watch) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.current = Some(watch);
        state.position = 0;
        state.duration = 0;
        state.seen = false;
        state.last_write = None;
    }

    /// The stored entry for a title, for `--resume`.
    pub fn entry(&self, id: u32, kind: &str) -> Option<Entry> {
        let state = self.state.lock().ok()?;
        state.history.entry(id, kind).cloned()
    }

    /// How far through the current file playback is, when mpv has reported
    /// both a position and a duration. Feeds the queue-next trigger.
    pub fn playing_fraction(&self) -> Option<f64> {
        let state = self.state.lock().ok()?;
        (state.seen && state.duration > 0)
            .then(|| (state.position as f64 / state.duration as f64).clamp(0.0, 1.0))
    }

    fn persist(&self, state: &mut State) {
        let Some(watch) = state.current.clone() else {
            return;
        };
        if !state.seen {
            return;
        }
        let (position, duration) = (state.position, state.duration);
        state.history.record(&watch, position, duration, now());
        state.last_write = Some(Instant::now());
        if let Err(e) = state.history.save(&self.path) {
            if !state.warned {
                state.warned = true;
                eprintln!("⚠  could not save watch history: {e:#}");
            }
        }
    }
}

impl crate::player::ProgressSink for Recorder {
    fn progress(&self, position: u64, duration: u64) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.position = position;
        state.duration = duration;
        state.seen = true;
        if let Some(last) = state.last_write {
            if last.elapsed() < WRITE_EVERY {
                return;
            }
        }
        self.persist(&mut state);
    }

    fn flush(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        self.persist(&mut state);
        // Whatever plays next gets its own `set_current`; without this, the
        // first position of the next episode would be written against the one
        // that just ended and undo the advance.
        state.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode_watch(episode: u32, next: Option<(u32, &str)>) -> Watch {
        Watch {
            id: 1396,
            kind: "tv",
            title: "Breaking Bad".into(),
            year: "2008".into(),
            poster: "/ggFH.jpg".into(),
            backdrop: "/tsRy.jpg".into(),
            season: Some(2),
            episode: Some(episode),
            episode_name: Some(format!("Episode {episode}")),
            next: next.map(|(n, name)| (n, name.to_string())),
        }
    }

    fn movie_watch(id: u32) -> Watch {
        Watch {
            id,
            kind: "movie",
            title: format!("Movie {id}"),
            year: "1999".into(),
            poster: String::new(),
            backdrop: String::new(),
            season: None,
            episode: None,
            episode_name: None,
            next: None,
        }
    }

    #[test]
    fn finishing_an_episode_advances_to_the_next_one() {
        let mut h = History::empty();
        h.record(&episode_watch(5, Some((6, "Peekaboo"))), 2_600, 2_760, 100);
        let e = h.entry(1396, "tv").unwrap();
        assert_eq!(e.episode, Some(6));
        assert_eq!(e.episode_name.as_deref(), Some("Peekaboo"));
        assert_eq!(e.position, 0);
        assert!(!e.finished, "the next episode has not been watched");
    }

    #[test]
    fn the_last_episode_of_a_season_stays_finished() {
        let mut h = History::empty();
        h.record(&episode_watch(13, None), 2_600, 2_760, 100);
        let e = h.entry(1396, "tv").unwrap();
        assert_eq!(e.episode, Some(13));
        assert!(e.finished);
    }

    #[test]
    fn ninety_percent_is_finished_and_anything_less_is_not() {
        let mut h = History::empty();
        // 89% — still watching.
        h.record(&movie_watch(550), 8_900, 10_000, 100);
        assert!(!h.entry(550, "movie").unwrap().finished);
        h.record(&movie_watch(550), 9_000, 10_000, 200);
        assert!(h.entry(550, "movie").unwrap().finished);
    }

    #[test]
    fn a_finished_movie_stays_in_the_list() {
        let mut h = History::empty();
        h.record(&movie_watch(550), 9_900, 10_000, 100);
        assert_eq!(h.items.len(), 1);
        assert!(h.entry(550, "movie").unwrap().finished);
    }

    #[test]
    fn an_unknown_duration_reports_no_progress() {
        let mut h = History::empty();
        h.record(&movie_watch(550), 300, 0, 100);
        let e = h.entry(550, "movie").unwrap();
        assert_eq!(e.progress(), 0.0);
        assert!(!e.finished);
    }

    #[test]
    fn the_last_minute_does_not_resume() {
        let mut h = History::empty();
        // 41 seconds left: finished, whatever the percentage says.
        h.record(&movie_watch(550), 9_959, 10_000, 100);
        assert_eq!(h.entry(550, "movie").unwrap().resume_position(), None);

        // Exactly 60 seconds left is inside the window too.
        h.record(&movie_watch(551), 9_940, 10_000, 100);
        assert_eq!(h.entry(551, "movie").unwrap().resume_position(), None);

        // 61 seconds left is still worth resuming.
        h.record(&movie_watch(552), 9_939, 10_000, 100);
        assert_eq!(
            h.entry(552, "movie").unwrap().resume_position(),
            Some(9_939)
        );
    }

    #[test]
    fn a_title_never_started_has_nowhere_to_resume() {
        let mut h = History::empty();
        h.record(&movie_watch(550), 0, 10_000, 100);
        assert_eq!(h.entry(550, "movie").unwrap().resume_position(), None);
    }

    #[test]
    fn one_entry_per_title_however_many_episodes() {
        let mut h = History::empty();
        h.record(&episode_watch(1, Some((2, "Two"))), 60, 2_760, 100);
        h.record(&episode_watch(2, Some((3, "Three"))), 60, 2_760, 200);
        assert_eq!(h.items.len(), 1);
        assert_eq!(h.entry(1396, "tv").unwrap().episode, Some(2));
    }

    #[test]
    fn the_file_is_capped_at_a_hundred_entries_least_recent_first() {
        let mut h = History::empty();
        for i in 0..120u32 {
            h.record(&movie_watch(i), 60, 10_000, 1_000 + i as u64);
        }
        assert_eq!(h.items.len(), MAX_ENTRIES);
        assert_eq!(h.items[0].id, 119, "newest first");
        assert!(h.entry(19, "movie").is_none(), "the oldest are dropped");
        assert!(h.entry(20, "movie").is_some());
    }

    #[test]
    fn recent_returns_newest_first() {
        let mut h = History::empty();
        h.record(&movie_watch(1), 60, 10_000, 100);
        h.record(&movie_watch(2), 60, 10_000, 300);
        h.record(&movie_watch(3), 60, 10_000, 200);
        let ids: Vec<u32> = h.recent(2).iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![2, 3]);
    }

    #[test]
    fn a_missing_or_corrupt_store_reads_as_empty() {
        assert!(History::load(Path::new("/nonexistent/dekho/history.json"))
            .items
            .is_empty());
    }
}
