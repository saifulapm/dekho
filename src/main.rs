//! dekho — search a movie or series and stream it straight into mpv.
//!
//! Nothing here talks to kojev.com. Metadata comes from TMDB with your own key,
//! releases come from Torrentio, and playback is a local torrent stream handed
//! to mpv as an ordinary HTTP URL.
//!
//! The design goal that shapes everything is *no buffering*. See `pick` for how
//! a release is chosen, `engine` for the throughput gate that vetoes one the
//! swarm cannot sustain, and `player` for mpv's own cushion on top.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use dekho::engine::{self, Engine, Probe};
use dekho::pick::{self, Filters, MAX_ATTEMPTS};
use dekho::player::{Event, Mpv};
use dekho::tmdb::{Episode, MediaType, SearchHit, Show, Tmdb};
use dekho::torrentio::{format_bps, format_bytes, Quality, Torrentio};

#[derive(Parser)]
#[command(
    name = "dekho",
    version,
    about = "Search movies and series, stream them straight into mpv"
)]
struct Cli {
    /// What to search for, e.g. `dekho breaking bad`
    #[arg(required = true)]
    query: Vec<String>,

    /// Highest quality to consider: 720p, 1080p, 4k
    #[arg(short = 'q', long, default_value = "4k")]
    quality: String,

    /// Skip releases needing more than this many Mbps sustained. This is what
    /// separates a streamable 4K WEB-DL from an unstreamable 4K remux.
    #[arg(long, default_value_t = 40)]
    max_bitrate: u64,

    /// Skip releases with fewer seeders than this
    #[arg(long, default_value_t = 4)]
    min_seeders: u32,

    /// Season to start from (series only; skips the picker)
    #[arg(short = 's', long)]
    season: Option<u32>,

    /// Episode to start from (series only; skips the picker)
    #[arg(short = 'e', long)]
    episode: Option<u32>,

    /// Play just the chosen episode instead of queueing the rest of the season
    #[arg(long)]
    no_next: bool,

    /// Take the top search match instead of asking. Makes the whole run
    /// non-interactive when combined with -s/-e.
    #[arg(short = '1', long)]
    first: bool,

    /// Resolve and buffer as usual, then print what would play and stop.
    /// Useful for checking which release the gate settles on.
    #[arg(long)]
    dry_run: bool,

    /// Where to keep downloaded pieces (default: $XDG_CACHE_HOME/dekho)
    #[arg(long)]
    download_dir: Option<PathBuf>,
}

/// A release that cleared the gate and is ready for mpv.
struct Playable {
    url: String,
    title: String,
    bitrate: Option<u64>,
    /// Kept so losing candidates can be dropped from the session.
    torrent_id: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let max_quality = Quality::parse_cap(&cli.quality)
        .with_context(|| format!("unknown --quality {:?}; try 720p, 1080p or 4k", cli.quality))?;
    let filters = Filters {
        max_quality,
        max_bitrate: cli.max_bitrate.saturating_mul(1_000_000),
        min_seeders: cli.min_seeders,
    };

    let key = std::env::var("TMDB_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .context(
            "TMDB_API_KEY is not set.\n\
             Get a free key at https://www.themoviedb.org/settings/api and export it:\n  \
             set -Ux TMDB_API_KEY <your key>",
        )?;

    let tmdb = Tmdb::new(key)?;
    let torrentio = Torrentio::new()?;
    let query = cli.query.join(" ");

    // --- find the title -----------------------------------------------------
    status(&format!("Searching TMDB for {query:?}…"));
    let mut hits = tmdb.search(&query).await?;
    anyhow::ensure!(!hits.is_empty(), "nothing on TMDB matched {query:?}");
    let hit = if cli.first {
        let top = hits.remove(0);
        status(&format!("→  {top}"));
        top
    } else {
        choose("What do you want to watch?", hits)?
    };

    // --- boot the engine ----------------------------------------------------
    let download_dir = cli
        .download_dir
        .clone()
        .unwrap_or_else(default_download_dir);
    status(&format!("Cache: {}", download_dir.display()));
    let engine = Engine::start(download_dir).await?;

    match hit.media_type {
        MediaType::Movie => play_movie(&tmdb, &torrentio, &engine, &filters, &hit, &cli).await,
        MediaType::Tv => play_series(&tmdb, &torrentio, &engine, &filters, &hit, &cli).await,
    }
}

/// Report what would play and stop, for `--dry-run`.
fn report_dry_run(playable: &Playable) -> Result<()> {
    println!("would play: {}", playable.title);
    println!("stream:     {}", playable.url);
    match playable.bitrate {
        Some(b) => println!("bitrate:    {}", format_bps(b)),
        None => println!("bitrate:    unknown"),
    }
    Ok(())
}

async fn play_movie(
    tmdb: &Tmdb,
    torrentio: &Torrentio,
    engine: &Engine,
    filters: &Filters,
    hit: &SearchHit,
    cli: &Cli,
) -> Result<()> {
    let movie = tmdb.movie(hit.id).await?;
    let label = if movie.year.is_empty() {
        movie.title.clone()
    } else {
        format!("{} ({})", movie.title, movie.year)
    };

    let playable = resolve(
        torrentio,
        engine,
        filters,
        &movie.imdb_id,
        MediaType::Movie,
        None,
        None,
        movie.runtime_secs,
        &label,
    )
    .await?;

    if cli.dry_run {
        return report_dry_run(&playable);
    }

    status(&format!("▶  {label}"));
    let mut mpv = Mpv::launch(&playable.url, &playable.title, playable.bitrate).await?;
    mpv.wait().await
}

async fn play_series(
    tmdb: &Tmdb,
    torrentio: &Torrentio,
    engine: &Engine,
    filters: &Filters,
    hit: &SearchHit,
    cli: &Cli,
) -> Result<()> {
    let show = tmdb.show(hit.id).await?;

    let season = match cli.season {
        Some(n) => *show
            .seasons
            .iter()
            .find(|s| s.number == n)
            .with_context(|| format!("{} has no season {n}", show.name))?,
        None if show.seasons.len() == 1 => show.seasons[0],
        None => choose("Which season?", show.seasons.clone())?,
    };

    let episodes = tmdb.episodes(&show, season.number).await?;
    anyhow::ensure!(
        !episodes.is_empty(),
        "TMDB lists no episodes for season {}",
        season.number
    );

    let start_at = match cli.episode {
        Some(n) => episodes
            .iter()
            .position(|e| e.number == n)
            .with_context(|| format!("season {} has no episode {n}", season.number))?,
        None => {
            let chosen = choose("Which episode?", episodes.clone())?;
            episodes
                .iter()
                .position(|e| e.number == chosen.number)
                .unwrap_or(0)
        }
    };

    // Everything from the chosen episode onward, so playback keeps going.
    let mut upcoming: VecDeque<Episode> = episodes.into_iter().skip(start_at).collect();
    let first = upcoming.pop_front().context("no episode to play")?;

    let playable = resolve_episode(torrentio, engine, filters, &show, &first).await?;

    if cli.dry_run {
        report_dry_run(&playable)?;
        println!(
            "then:       {}",
            upcoming
                .front()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "(end of season)".into())
        );
        return Ok(());
    }

    status(&format!("▶  {} — {}", show.name, first));
    let mut mpv = Mpv::launch(&playable.url, &playable.title, playable.bitrate).await?;

    if cli.no_next {
        return mpv.wait().await;
    }

    // mpv emits start-file for the episode we just launched. Consume it, so the
    // loop below only reacts to *advancing* to a queued episode.
    wait_for_start(&mut mpv).await;

    // Keep exactly one episode queued ahead. Appending earlier would start
    // extra torrents that compete for bandwidth with the one being watched;
    // appending later would leave a gap at the episode boundary.
    loop {
        if let Some(next) = upcoming.front().cloned() {
            match resolve_episode(torrentio, engine, filters, &show, &next).await {
                Ok(p) => {
                    status(&format!("⏭  queued {next}"));
                    mpv.append(&p.url, &p.title).await?;
                    upcoming.pop_front();
                }
                Err(e) => {
                    status(&format!("⚠  skipping {next}: {e}"));
                    upcoming.pop_front();
                    continue;
                }
            }
        }

        if mpv.has_exited() {
            break;
        }

        match mpv.next_event().await {
            None => break,
            Some(Event::Idle) => break,
            // Advanced to the queued episode — time to prepare the next one.
            Some(Event::StartFile) => continue,
            Some(Event::EndFile { reason }) if reason == "quit" => break,
            Some(Event::EndFile { .. }) => {
                if upcoming.is_empty() {
                    break;
                }
            }
        }
    }

    mpv.wait().await
}

async fn resolve_episode(
    torrentio: &Torrentio,
    engine: &Engine,
    filters: &Filters,
    show: &Show,
    ep: &Episode,
) -> Result<Playable> {
    let label = if show.year.is_empty() {
        format!("{} — {}", show.name, ep)
    } else {
        format!("{} ({}) — {}", show.name, show.year, ep)
    };
    resolve(
        torrentio,
        engine,
        filters,
        &show.imdb_id,
        MediaType::Tv,
        Some(ep.season),
        Some(ep.number),
        ep.runtime_secs,
        &label,
    )
    .await
}

/// Find a release, prove the swarm can sustain it, and return a URL for mpv.
///
/// Candidates are walked best-first and each is measured. The first to clear
/// its own bitrate with headroom wins. If none does, the fastest one measured
/// is used anyway with a warning — a stuttering stream beats no stream, but the
/// user should know which they are getting.
#[allow(clippy::too_many_arguments)]
async fn resolve(
    torrentio: &Torrentio,
    engine: &Engine,
    filters: &Filters,
    imdb_id: &str,
    media_type: MediaType,
    season: Option<u32>,
    episode: Option<u32>,
    runtime_secs: u32,
    label: &str,
) -> Result<Playable> {
    status(&format!("Looking up releases for {label}…"));
    let all = torrentio
        .candidates(imdb_id, media_type.torrentio(), season, episode)
        .await?;
    anyhow::ensure!(!all.is_empty(), "Torrentio has no releases for {label}");

    let found = all.len();
    let shortlist = pick::shortlist(all, filters, runtime_secs);
    anyhow::ensure!(
        !shortlist.is_empty(),
        "none of the {found} releases for {label} fit the filters \
         (try a higher --max-bitrate, a lower --min-seeders, or -q 1080p)"
    );

    // Best fallback seen so far, in case nothing clears the gate.
    let mut fallback: Option<(u64, Playable)> = None;

    for candidate in pick::attempt_order(&shortlist, MAX_ATTEMPTS) {
        let needed = candidate.required_bps(runtime_secs);
        status(&format!(
            "Trying {} · {} · {} seeders{}",
            candidate.quality.label(),
            candidate
                .size_bytes
                .map(format_bytes)
                .unwrap_or_else(|| "size unknown".into()),
            candidate.seeders,
            needed
                .map(|b| format!(" · needs {}", format_bps(b)))
                .unwrap_or_default(),
        ));

        let added = match engine.add(&candidate.magnet).await {
            Ok(a) => a,
            Err(e) => {
                status(&format!("   could not add: {e}"));
                continue;
            }
        };
        let file_idx = match engine::choose_file(&added.files, candidate.file_idx, season, episode)
        {
            Ok(i) => i,
            Err(e) => {
                status(&format!("   no playable file: {e}"));
                engine.forget(added.id).await;
                continue;
            }
        };
        // Season packs otherwise fetch every episode at once, splitting
        // bandwidth away from the one on screen.
        engine.only_file(&added.handle, file_idx).await;

        let url = engine.stream_url(added.id, file_idx);
        let file_name = added
            .files
            .iter()
            .find(|f| f.idx == file_idx)
            .map(|f| f.name.clone())
            .unwrap_or_else(|| candidate.title.clone());

        let probe = engine
            .probe(&added, file_idx, &url, needed, |s| {
                progress(&format!(
                    "   buffering {} · {} · {} peer{} ({} known)",
                    format_bytes(s.buffered),
                    format_bps(s.rate_bps),
                    s.live_peers,
                    if s.live_peers == 1 { "" } else { "s" },
                    s.seen_peers,
                ));
            })
            .await;

        clear_progress();

        match probe {
            Ok(Probe::Ready { rate_bps, buffered }) => {
                status(&format!(
                    "   ready · {} buffered at {}",
                    format_bytes(buffered),
                    format_bps(rate_bps)
                ));
                // This one won; stop any earlier also-ran from stealing
                // bandwidth from it.
                if let Some((_, old)) = fallback {
                    engine.forget(old.torrent_id).await;
                }
                return Ok(Playable {
                    url,
                    title: label.to_string(),
                    bitrate: needed,
                    torrent_id: added.id,
                });
            }
            Ok(Probe::TooSlow {
                rate_bps,
                buffered,
                live_peers,
                seen_peers,
            }) => {
                status(&format!(
                    "   too slow · {} sustained, {} buffered, {live_peers} peers \
                     ({seen_peers} known) — trying a lighter release",
                    format_bps(rate_bps),
                    format_bytes(buffered),
                ));
                let better = fallback
                    .as_ref()
                    .map(|(r, _)| rate_bps > *r)
                    .unwrap_or(true);
                if better {
                    // Keep the previous fallback's torrent from competing for
                    // bandwidth with everything that comes after it.
                    if let Some((_, old)) = fallback.replace((
                        rate_bps,
                        Playable {
                            url,
                            title: format!("{label} [{file_name}]"),
                            bitrate: needed,
                            torrent_id: added.id,
                        },
                    )) {
                        engine.forget(old.torrent_id).await;
                    }
                } else {
                    engine.forget(added.id).await;
                }
            }
            Err(e) => {
                status(&format!("   probe failed: {e}"));
                engine.forget(added.id).await;
            }
        }
    }

    match fallback {
        Some((rate, playable)) => {
            status(&format!(
                "⚠  No release cleared the smoothness check. Playing the fastest one \
                 ({} sustained) — it may buffer.",
                format_bps(rate)
            ));
            Ok(playable)
        }
        None => anyhow::bail!(
            "could not stream {label}: no release could be started. \
             Try -q 1080p or --min-seeders 1."
        ),
    }
}

/// Consume events until the first `start-file`, so the caller's loop only sees
/// transitions between queued entries. Bounded, so a silent mpv cannot hang us.
async fn wait_for_start(mpv: &mut Mpv) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return;
        }
        match tokio::time::timeout(left, mpv.next_event()).await {
            Ok(Some(Event::StartFile)) | Ok(None) | Err(_) => return,
            Ok(Some(_)) => continue,
        }
    }
}

fn choose<T: std::fmt::Display>(prompt: &str, options: Vec<T>) -> Result<T> {
    use inquire::error::InquireError;
    match inquire::Select::new(prompt, options)
        .with_page_size(15)
        .prompt()
    {
        Ok(v) => Ok(v),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
            std::process::exit(130)
        }
        Err(e) => Err(e).context("reading your selection"),
    }
}

fn default_download_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("dekho");
        }
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => PathBuf::from(home).join(".cache").join("dekho"),
        _ => std::env::temp_dir().join("dekho"),
    }
}

/// A durable status line. Goes to stderr so stdout stays clean.
fn status(msg: &str) {
    clear_progress();
    eprintln!("{msg}");
}

/// A transient, overwritten progress line.
fn progress(msg: &str) {
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\x1b[2K{msg}");
    let _ = err.flush();
}

fn clear_progress() {
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\x1b[2K");
    let _ = err.flush();
}
