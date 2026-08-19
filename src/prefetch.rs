//! The poster cache, so a panel never has to do HTTP itself.
//!
//! A rail of twenty posters is twenty TLS handshakes against TMDB's CDN, which
//! is the one thing a shell-based UI cannot do quickly. dekho downloads them
//! concurrently, keeps them under `$XDG_CACHE_HOME`, and answers with local
//! paths. It is called on every panel open, so anything already on disk is
//! reported rather than fetched again.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};

const BASE: &str = "https://image.tmdb.org/t/p";

/// TMDB's image sizes. Anything else is a 404 from their CDN — and it also
/// becomes a directory name here, so it is checked rather than passed through.
pub const SIZES: &[&str] = &["w92", "w154", "w185", "w342", "w500", "w780", "original"];

/// Downloads in flight. A rail is ~20 posters; eight at a time fills a home
/// connection without opening a socket per poster.
const MAX_IN_FLIGHT: usize = 8;

/// Where images of one size are kept.
pub fn dir_for(size: &str) -> PathBuf {
    crate::xdg::cache_home()
        .join("dekho")
        .join("img")
        .join(size)
}

pub fn check_size(size: &str) -> Result<()> {
    anyhow::ensure!(
        SIZES.contains(&size),
        "unknown --size {size:?}; use one of {}",
        SIZES.join(", ")
    );
    Ok(())
}

/// The filename a TMDB image path maps to.
///
/// The result goes straight into a filesystem path, so anything that is not a
/// single `/name.ext` is refused rather than sanitised: TMDB never emits one,
/// which makes a path with a second segment or a `..` in it either a caller bug
/// or an attempt to write outside the cache.
pub fn basename(path: &str) -> Result<&str> {
    let name = path.strip_prefix('/').unwrap_or(path);
    anyhow::ensure!(!name.is_empty(), "empty image path");
    anyhow::ensure!(
        !name.contains('/'),
        "image path {path:?} has more than one segment"
    );
    anyhow::ensure!(
        !name.contains(".."),
        "image path {path:?} traverses directories"
    );
    Ok(name)
}

/// Download whatever is missing and answer with a path per image.
pub async fn run(size: &str, paths: &[String]) -> Result<Value> {
    check_size(size)?;
    let dir = dir_for(size);

    // Validate the whole request before touching the filesystem, so a bad path
    // writes nothing at all.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut wanted: Vec<(String, PathBuf)> = Vec::new();
    for path in paths {
        let name = basename(path)?;
        if seen.insert(name) {
            wanted.push((path.clone(), dir.join(name)));
        }
    }

    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let mut files = serde_json::Map::new();
    let mut missing: Vec<(String, PathBuf)> = Vec::new();
    for (path, dest) in wanted {
        if dest.exists() {
            files.insert(path, json!(dest.display().to_string()));
        } else {
            missing.push((path, dest));
        }
    }

    if !missing.is_empty() {
        let http = reqwest::Client::builder()
            .user_agent(concat!("dekho/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("building the image HTTP client")?;

        // A shared cursor rather than fixed batches: one slow image would
        // otherwise hold up the seven downloaded alongside it.
        let missing = Arc::new(missing);
        let next = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..MAX_IN_FLIGHT.min(missing.len()) {
            let (http, missing, next) = (http.clone(), missing.clone(), next.clone());
            let size = size.to_string();
            workers.push(tokio::spawn(async move {
                let mut done: Vec<(String, String)> = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((path, dest)) = missing.get(i) else {
                        break;
                    };
                    // A dead image is simply absent from `files`. This call is
                    // made on every panel open; one 404 must not fail it.
                    if fetch(&http, &size, path, dest).await.is_ok() {
                        done.push((path.clone(), dest.display().to_string()));
                    }
                }
                done
            }));
        }
        for worker in workers {
            if let Ok(done) = worker.await {
                for (path, local) in done {
                    files.insert(path, json!(local));
                }
            }
        }
    }

    Ok(json!({
        "dir": dir.display().to_string(),
        "files": Value::Object(files),
    }))
}

async fn fetch(http: &reqwest::Client, size: &str, path: &str, dest: &Path) -> Result<()> {
    let url = format!(
        "{BASE}/{size}{}{path}",
        if path.starts_with('/') { "" } else { "/" }
    );
    let res = http.get(&url).send().await?;
    anyhow::ensure!(res.status().is_success(), "HTTP {}", res.status());
    let bytes = res.bytes().await?;

    // Write beside the target and rename in: a killed run would otherwise leave
    // a truncated JPEG behind, and every later call would treat it as cached
    // and never fix it. The pid keeps two dekho processes prefetching the same
    // poster off each other's temp file.
    let part = dest.with_extension(format!("{}.part", std::process::id()));
    std::fs::write(&part, &bytes)?;
    std::fs::rename(&part, dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_tmdb_path_yields_its_filename() {
        assert_eq!(basename("/pB8B.jpg").unwrap(), "pB8B.jpg");
        assert_eq!(basename("pB8B.jpg").unwrap(), "pB8B.jpg");
    }

    #[test]
    fn a_traversing_path_is_refused() {
        assert!(basename("../../etc/passwd").is_err());
        assert!(basename("/../../etc/passwd").is_err());
        assert!(basename("/..").is_err());
    }

    #[test]
    fn a_path_with_a_second_segment_is_refused() {
        assert!(basename("/a/b.jpg").is_err());
        assert!(basename("/t/p/w342/x.jpg").is_err());
    }

    #[test]
    fn an_empty_path_is_refused() {
        assert!(basename("").is_err());
        assert!(basename("/").is_err());
    }

    #[test]
    fn only_tmdbs_own_sizes_are_accepted() {
        assert!(check_size("w342").is_ok());
        assert!(check_size("original").is_ok());
        assert!(check_size("w999").is_err());
        // Would otherwise become a directory name.
        assert!(check_size("../../..").is_err());
    }

    #[test]
    fn the_cache_directory_is_per_size() {
        assert!(dir_for("w342").ends_with("dekho/img/w342"));
    }
}
