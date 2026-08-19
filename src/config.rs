//! The optional config file: the TMDB key, the piece-cache budget, and when
//! to queue the next episode.
//!
//! The environment still wins for the key. `TMDB_API_KEY` is how the tool was
//! documented, and it is how a one-off run overrides everything. The file
//! exists because a desktop panel spawns dekho without a login shell's
//! exported environment, so the key has to be readable from somewhere on disk
//! too. For the other keys the command-line flag wins over the file.
//!
//! Parsed by hand, like `tmdb`'s percent-encoder: this is a two-line file of
//! `key = "value"`, and a TOML crate would be a dependency for that alone.
//! Unknown keys are ignored rather than rejected, so a config file written for
//! a later dekho still works with this one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const KEY: &str = "tmdb_api_key";

/// Where the config file lives.
pub fn path() -> PathBuf {
    crate::xdg::config_home().join("dekho").join("config.toml")
}

/// The TMDB key, from the environment first and the config file second.
pub fn tmdb_key() -> Result<String> {
    if let Some(key) = std::env::var("TMDB_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
    {
        return Ok(key);
    }

    let path = path();
    file_key(&path).with_context(|| {
        format!(
            "TMDB_API_KEY is not set.\n\
             Get a free key at https://www.themoviedb.org/settings/api and export it:\n  \
             set -Ux TMDB_API_KEY <your key>\n\
             or put it in {}:\n  \
             tmdb_api_key = \"<your key>\"",
            path.display()
        )
    })
}

/// The key stored in a config file. A missing file is not an error — most
/// installs only ever set the environment variable.
fn file_key(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    value_of(&raw, KEY)
}

/// Read any key out of the config file. A missing file is simply no value.
pub fn get(key: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path()).ok()?;
    value_of(&raw, key)
}

/// `cache_max_gb`: the piece-cache budget in GiB, 0 meaning unlimited.
pub fn cache_max_gb() -> Option<u64> {
    get("cache_max_gb").and_then(|v| v.parse().ok())
}

/// `queue_next`: when a series queues the following episode — `auto`,
/// `immediate`, or a percentage like `50%`. Parsed by `player::QueueWhen`.
pub fn queue_next() -> Option<String> {
    get("queue_next")
}

/// Read one key out of config-file text.
fn value_of(src: &str, key: &str) -> Option<String> {
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        let value = unquote(value.trim());
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Strip one layer of matching quotes, leaving anything else alone.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_quoted_value() {
        assert_eq!(
            value_of("tmdb_api_key = \"abc123\"\n", KEY).as_deref(),
            Some("abc123")
        );
        assert_eq!(
            value_of("tmdb_api_key = 'abc123'\n", KEY).as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn quotes_are_optional_and_whitespace_is_trimmed() {
        assert_eq!(
            value_of("   tmdb_api_key   =   abc123   \n", KEY).as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let src = "# dekho config\n\n  # tmdb_api_key = \"wrong\"\ntmdb_api_key = \"right\"\n";
        assert_eq!(value_of(src, KEY).as_deref(), Some("right"));
    }

    #[test]
    fn unknown_keys_are_ignored_rather_than_rejected() {
        // A config file written by a later dekho must still work here.
        let src = "player = \"mpv\"\ntmdb_api_key = \"abc123\"\nsubtitles = true\n";
        assert_eq!(value_of(src, KEY).as_deref(), Some("abc123"));
    }

    #[test]
    fn a_value_containing_an_equals_sign_survives() {
        // Splitting on the first `=` only, so a base64-ish key keeps its tail.
        assert_eq!(
            value_of("tmdb_api_key = \"ab=c1==\"", KEY).as_deref(),
            Some("ab=c1==")
        );
    }

    #[test]
    fn an_empty_or_absent_key_is_none() {
        assert_eq!(value_of("tmdb_api_key = \"\"\n", KEY), None);
        assert_eq!(value_of("# nothing here\n", KEY), None);
        assert_eq!(value_of("", KEY), None);
    }

    #[test]
    fn a_missing_config_file_is_not_an_error() {
        assert_eq!(file_key(Path::new("/nonexistent/dekho/config.toml")), None);
    }

    #[test]
    fn only_one_layer_of_quotes_comes_off() {
        assert_eq!(unquote("\"'x'\""), "'x'");
        assert_eq!(unquote("\"unbalanced"), "\"unbalanced");
        assert_eq!(unquote("\""), "\"");
    }
}
