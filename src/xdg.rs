//! Where dekho keeps things on disk.
//!
//! Hand-rolled rather than taken from a crate: three environment variables with
//! one fallback each does not justify a dependency, and the `$HOME`-unset case
//! has to be spelled out here either way.

use std::path::PathBuf;

/// `$XDG_CONFIG_HOME`, or `~/.config`.
pub fn config_home() -> PathBuf {
    base("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_CACHE_HOME`, or `~/.cache`.
pub fn cache_home() -> PathBuf {
    base("XDG_CACHE_HOME", ".cache")
}

/// `$XDG_STATE_HOME`, or `~/.local/state`.
pub fn state_home() -> PathBuf {
    base("XDG_STATE_HOME", ".local/state")
}

fn base(var: &str, fallback: &str) -> PathBuf {
    if let Ok(dir) = std::env::var(var) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => PathBuf::from(home).join(fallback),
        // No HOME at all: a temp directory is wrong but harmless, whereas a
        // path relative to the working directory would litter whatever
        // directory dekho happened to be launched from.
        _ => std::env::temp_dir(),
    }
}
