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

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Where cached answers live, beside the image cache.
pub fn dir() -> PathBuf {
    crate::xdg::cache_home().join("dekho").join("api")
}

/// Per-verb time-to-live, in seconds.
///
/// The figures follow how fast each answer actually goes stale: a title's
/// details survive a day, a search a quarter hour. `genres` and `languages`
/// are compiled in and never reach the network, so they need no entry.
pub fn ttl_secs(verb: &str) -> Option<u64> {
    match verb {
        "search" => Some(15 * 60),
        "trending" => Some(60 * 60),
        "discover" => Some(60 * 60),
        "title" => Some(24 * 60 * 60),
        "episodes" => Some(6 * 60 * 60),
        _ => None,
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
pub struct Cached {
    pub value: Value,
    pub age_secs: u64,
}

/// Whether an entry of `age_secs` still answers for a verb with `ttl`.
pub fn is_fresh(age_secs: u64, ttl_secs: u64) -> bool {
    age_secs <= ttl_secs
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn read(dir: &Path, key: &str) -> Option<Cached> {
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
pub fn write(dir: &Path, key: &str, value: &Value) {
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
pub fn mark(mut value: Value, age_secs: u64, stale: bool) -> Value {
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
