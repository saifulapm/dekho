//! Disk cache for `dekho api` answers, so the panel opens without waiting on
//! TMDB.
//!
//! The desktop panel fires four verbs plus a prefetch on every open. On a slow
//! link that is a round trip per rail; when the link flaps it was a red line
//! per rail. So each verb's finished JSON object is cached under
//! `$XDG_CACHE_HOME/dekho/api/`, keyed by the verb and its arguments, with a
//! TTL per verb — hours for a title's details, minutes for a search.
//!
//! Two rules make it useful rather than just fast:
//!
//! - **Stale beats an error.** When the network fails, an expired entry is
//!   served instead of `{"error":…}` — a hub showing yesterday's trending
//!   beats a hub showing a red line. The payload says so (`"stale": true`),
//!   so a caller that cares can tell.
//! - **Cache answers are marked.** Anything served from disk carries
//!   `"cached": true` and `"age_secs"`. Fresh network answers carry neither,
//!   so the existing contract is untouched.
//!
//! `--refresh` skips the fresh-cache read (the answer is still written back,
//! and still falls back to stale if the network refuses). `history` and
//! `prefetch` never come through here — they are local and stay that way.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Value};

/// Where cached answers live, beside the image cache.
fn dir() -> PathBuf {
    crate::xdg::cache_home().join("dekho").join("api")
}

/// Per-verb time-to-live, in seconds.
///
/// The figures follow how fast each answer actually goes stale: a title's
/// details survive a day, a search a quarter hour. `genres` and `languages`
/// are compiled in and never reach the network, so they need no entry.
///
/// The people-shaped answers last longest, because they change slowest. A
/// film's trailer list is settled the week it ships; an actor's filmography
/// gains a row every few months, and serving one three days old is invisible
/// next to paying for it on every click.
pub fn ttl_secs(verb: &str) -> Option<u64> {
    match verb {
        "search" => Some(15 * 60),
        "trending" => Some(60 * 60),
        "discover" => Some(60 * 60),
        "title" => Some(24 * 60 * 60),
        "episodes" => Some(6 * 60 * 60),
        "videos" => Some(24 * 60 * 60),
        "person" => Some(3 * 24 * 60 * 60),
        _ => None,
    }
}

/// Answer through the cache: a fresh entry without the network, the network
/// otherwise, and — when that fails — an expired entry rather than an error.
///
/// `plan` of `None` is a verb that must stay live, in which case this is just
/// the fetch. `refresh` skips the fresh read but keeps the stale fallback: a
/// forced refresh that cannot reach TMDB is still better answered with
/// yesterday's data than with a red line.
///
/// `fetch` is a future, not a closure, and it is only awaited on a miss — so a
/// cache hit never builds an HTTP client or reads the API key.
pub async fn serve<F>(plan: Option<&(String, u64)>, refresh: bool, fetch: F) -> Result<Value>
where
    F: Future<Output = Result<Value>>,
{
    let dir = dir();
    if !refresh {
        if let Some((key, ttl)) = plan {
            if let Some(hit) = read(&dir, key) {
                if is_fresh(hit.age_secs, *ttl) {
                    return Ok(mark(hit.value, hit.age_secs, false));
                }
            }
        }
    }

    match fetch.await {
        Ok(value) => {
            if let Some((key, _)) = plan {
                write(&dir, key, &value);
            }
            Ok(value)
        }
        Err(e) => {
            if let Some((key, _)) = plan {
                if let Some(hit) = read(&dir, key) {
                    return Ok(mark(hit.value, hit.age_secs, true));
                }
            }
            Err(e)
        }
    }
}

/// A cache key from the verb and its arguments. FNV-1a over the joined parts —
/// hand-rolled like the percent-encoder, because a hash crate for one filename
/// is not worth a dependency. The verb prefixes the filename so `cache clear`
/// debugging stays legible.
pub fn key(parts: &[&str]) -> String {
    let joined = parts.join("\x1f");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in joined.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{}-{hash:016x}", parts.first().unwrap_or(&"v"))
}

/// A cached answer and how old it is.
struct Cached {
    pub value: Value,
    pub age_secs: u64,
}

/// Whether an entry of `age_secs` still answers for a verb with `ttl`.
fn is_fresh(age_secs: u64, ttl_secs: u64) -> bool {
    age_secs <= ttl_secs
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read(dir: &Path, key: &str) -> Option<Cached> {
    let raw = std::fs::read_to_string(dir.join(format!("{key}.json"))).ok()?;
    let stored: Value = serde_json::from_str(&raw).ok()?;
    let saved = stored.get("saved").and_then(Value::as_u64)?;
    let value = stored.get("value")?.clone();
    Some(Cached {
        value,
        age_secs: now_secs().saturating_sub(saved),
    })
}

/// Best-effort: a cache that cannot be written is a cache miss next time, not
/// an error now.
fn write(dir: &Path, key: &str, value: &Value) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let stored = json!({ "saved": now_secs(), "value": value });
    let path = dir.join(format!("{key}.json"));
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, stored.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Mark an answer as served from disk. `stale` means it had expired and is
/// only being served because the network failed.
fn mark(mut value: Value, age_secs: u64, stale: bool) -> Value {
    if let Some(map) = value.as_object_mut() {
        map.insert("cached".into(), json!(true));
        map.insert("age_secs".into(), json!(age_secs));
        if stale {
            map.insert("stale".into(), json!(true));
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_and_argument_sensitive() {
        let a = key(&["search", "fight club"]);
        let b = key(&["search", "fight club"]);
        let c = key(&["search", "fight clubs"]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("search-"), "verb stays legible: {a}");
    }

    #[test]
    fn keys_do_not_collide_on_joined_boundaries() {
        // "ab"+"c" and "a"+"bc" must differ — the separator is the point.
        assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]));
    }

    #[test]
    fn ttls_cover_the_networked_verbs_and_nothing_else() {
        assert!(ttl_secs("search").is_some());
        assert!(ttl_secs("trending").is_some());
        assert!(ttl_secs("discover").is_some());
        assert!(ttl_secs("title").is_some());
        assert!(ttl_secs("episodes").is_some());
        assert!(ttl_secs("videos").is_some());
        assert!(ttl_secs("person").is_some());
        // Local verbs must never be served from this cache.
        assert_eq!(ttl_secs("history"), None);
        assert_eq!(ttl_secs("prefetch"), None);
        assert_eq!(ttl_secs("genres"), None);
        assert_eq!(ttl_secs("languages"), None);
        assert_eq!(ttl_secs("cache"), None);
    }

    #[test]
    fn title_answers_live_longer_than_searches() {
        assert!(ttl_secs("title").unwrap() > ttl_secs("search").unwrap());
    }

    #[test]
    fn a_filmography_outlives_what_is_trending() {
        // A career moves in months; trending moves in hours.
        assert!(ttl_secs("person").unwrap() > ttl_secs("trending").unwrap());
        assert!(ttl_secs("person").unwrap() >= ttl_secs("title").unwrap());
    }

    #[test]
    fn freshness_is_a_simple_ttl_comparison() {
        assert!(is_fresh(0, 900));
        assert!(is_fresh(900, 900));
        assert!(!is_fresh(901, 900));
    }

    #[test]
    fn marking_adds_fields_without_touching_the_rest() {
        let v = json!({"items": [1, 2, 3]});
        let m = mark(v, 42, false);
        assert_eq!(m["items"], json!([1, 2, 3]));
        assert_eq!(m["cached"], json!(true));
        assert_eq!(m["age_secs"], json!(42));
        assert!(m.get("stale").is_none(), "fresh-from-cache is not stale");
    }

    #[test]
    fn a_stale_serve_says_so() {
        let m = mark(json!({"items": []}), 90_000, true);
        assert_eq!(m["stale"], json!(true));
    }

    /// A private cache dir per test, since `serve` uses the real one only
    /// through `dir()` — these exercise `read`/`write`/`mark` directly.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dekho-apicache-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_fresh_entry_answers_without_the_network() {
        let dir = scratch("fresh");
        let key = key(&["person", "17419"]);
        write(&dir, &key, &json!({"name": "Bryan Cranston"}));
        let hit = read(&dir, &key).expect("a hit");
        assert!(is_fresh(hit.age_secs, ttl_secs("person").unwrap()));
        let served = mark(hit.value, hit.age_secs, false);
        assert_eq!(served["name"], json!("Bryan Cranston"));
        assert_eq!(served["cached"], json!(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expired_entry_is_still_there_to_serve_when_the_network_fails() {
        // The stale path is exactly this: the entry is too old to answer
        // normally, and is served anyway rather than an error.
        let dir = scratch("stale");
        let key = key(&["videos", "550", "movie"]);
        write(&dir, &key, &json!({"items": []}));
        let hit = read(&dir, &key).expect("a hit");
        assert!(!is_fresh(hit.age_secs + 999_999, 60));
        assert_eq!(mark(hit.value, 999_999, true)["stale"], json!(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_and_read_round_trip_with_an_age() {
        let dir = std::env::temp_dir().join(format!("dekho-apicache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let value = json!({"items": [{"id": 550}]});
        write(&dir, &key(&["search", "x"]), &value);
        let hit = read(&dir, &key(&["search", "x"])).expect("a hit");
        assert_eq!(hit.value, value);
        assert!(hit.age_secs < 5, "just written");
        assert!(read(&dir, &key(&["search", "y"])).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
