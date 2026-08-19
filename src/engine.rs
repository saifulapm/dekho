//! Torrent engine: add a magnet, choose the right file, and prove the swarm can
//! actually sustain playback before we hand anything to mpv.
//!
//! Two things make this stream smoothly rather than stutter:
//!
//! 1. **Sequential priority.** librqbit feeds `priority_pieces` from every open
//!    stream's playhead, so the moment a stream is opened the swarm starts
//!    fetching *ahead of where you are watching* instead of rarest-first. We get
//!    that for free by reading through the HTTP stream endpoint.
//!
//! 2. **A throughput gate.** A 74 GB 4K remux with 40 seeders looks like the
//!    best release in the list and is unwatchable; a 1080p WEB-DL with 600
//!    seeders is not. Size ÷ runtime gives the bitrate a release *needs*, and
//!    `probe` measures what the swarm actually *delivers*. Only a release that
//!    clears its own bitrate with headroom gets played — everything else is
//!    dropped and the next candidate is tried. This is the single biggest
//!    difference between "high quality" and "high quality that plays".
//!
//! The probe is not wasted work: every byte it pulls is written to disk by the
//! torrent, so it doubles as the pre-buffer and playback starts instantly.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use librqbit::api::Api;
use librqbit::http_api::{HttpApi, HttpApiOptions};
use librqbit::{AddTorrent, AddTorrentOptions, Session};
use librqbit_dualstack_sockets::{BindOpts, TcpListener as DualstackTcpListener};

/// How much headroom over the release's own bitrate the swarm must show before
/// we commit. 1.25x absorbs the normal sawtooth of peer churn without being so
/// strict that good releases get rejected.
const RATE_SAFETY_FACTOR: f64 = 1.25;

/// Seconds of video buffered before mpv is launched. mpv keeps buffering after
/// this, so it is a floor on the head start, not the total cushion.
const PREBUFFER_SECS: u64 = 45;

/// Hard ceiling on the pre-buffer, so a 4K remux does not make us wait minutes.
const PREBUFFER_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// How long a single candidate gets to prove itself. Peer discovery over DHT
/// and trackers is not instant, so this has to be patient — but the probe exits
/// the moment the gate is cleared, so a healthy swarm costs only a second or two.
const PROBE_BUDGET: Duration = Duration::from_secs(30);

/// How long a candidate gets before a clearly-hopeless rate ends it early.
/// Long enough for DHT and tracker announces to have produced peers.
const EARLY_ABANDON_AFTER: Duration = Duration::from_secs(12);

/// Window the sustained rate is measured over. Cumulative-since-start would be
/// dragged down by the seconds before any peer connected.
const RATE_WINDOW: Duration = Duration::from_secs(5);

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "m4v", "mov", "ts", "webm", "mpg", "mpeg", "wmv", "flv",
];

/// A file inside a torrent.
#[derive(Clone, Debug)]
pub struct TorrentFile {
    pub idx: usize,
    pub name: String,
    pub len: u64,
}

impl TorrentFile {
    fn is_video(&self) -> bool {
        let lower = self.name.to_ascii_lowercase();
        VIDEO_EXTENSIONS
            .iter()
            .any(|ext| lower.ends_with(&format!(".{ext}")))
    }
}

/// A torrent that has resolved its metadata and is ready to stream.
pub struct Added {
    pub id: usize,
    pub files: Vec<TorrentFile>,
}

/// What the probe learned about a candidate.
#[derive(Debug)]
pub enum Probe {
    /// Cleared the gate. `rate_bps` is the sustained throughput measured.
    Ready { rate_bps: u64, buffered: u64 },
    /// Ran out of budget. `rate_bps` is the best sustained rate seen.
    TooSlow { rate_bps: u64, buffered: u64 },
}

pub struct Engine {
    session: Arc<Session>,
    base_url: String,
    http: reqwest::Client,
}

impl Engine {
    /// Boot a torrent session and a loopback HTTP server in front of it.
    ///
    /// mpv gets a plain `http://127.0.0.1:…` URL, which means it can issue
    /// Range requests and therefore seek — something a pipe or FIFO could not
    /// offer. The port is ephemeral and bound to loopback only.
    pub async fn start(download_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&download_dir)
            .with_context(|| format!("creating download dir {}", download_dir.display()))?;

        let session = Session::new(download_dir)
            .await
            .context("starting the torrent session")?;

        // Port 0 lets the OS pick, and loopback keeps the stream off the
        // network — this server exposes file contents with no auth.
        let listener = DualstackTcpListener::bind_tcp(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            BindOpts {
                request_dualstack: false,
                ..Default::default()
            },
        )
        .context("binding the local stream server")?;
        let port = listener.bind_addr().port();

        // Third argument is the log-line broadcast, which only exists because
        // the `tracing-subscriber-utils` feature is on (see Cargo.toml). We
        // stream no logs, so it stays None.
        let api = Api::new(session.clone(), None, None);
        let http_api = HttpApi::new(
            api,
            Some(HttpApiOptions {
                // Nothing outside this process should be able to add or delete
                // torrents through it, and we never create torrents.
                read_only: true,
                allow_create: false,
                ..Default::default()
            }),
        );
        tokio::spawn(http_api.make_http_api_and_run(listener, None));

        let http = reqwest::Client::builder()
            .user_agent(concat!("dekho/", env!("CARGO_PKG_VERSION")))
            // No timeout: a probe read legitimately blocks while pieces arrive.
            .build()
            .context("building the stream HTTP client")?;

        Ok(Self {
            session,
            base_url: format!("http://127.0.0.1:{port}"),
            http,
        })
    }

    /// Add a magnet and wait for its metadata to resolve.
    pub async fn add(&self, magnet: &str) -> Result<Added> {
        let handle = self
            .session
            .add_torrent(
                AddTorrent::from_url(magnet),
                Some(AddTorrentOptions {
                    // Resume rather than re-download when the same release was
                    // played before.
                    overwrite: true,
                    ..Default::default()
                }),
            )
            .await
            .context("adding the torrent")?
            .into_handle()
            .context("the torrent was not added")?;

        handle
            .wait_until_initialized()
            .await
            .context("resolving torrent metadata from the swarm")?;

        let files = handle.with_metadata(|m| {
            m.file_infos
                .iter()
                .enumerate()
                .map(|(idx, f)| TorrentFile {
                    idx,
                    name: f.relative_filename.to_string_lossy().to_string(),
                    len: f.len,
                })
                .collect::<Vec<_>>()
        })?;

        Ok(Added {
            id: handle.id(),
            files,
        })
    }

    /// The URL mpv opens. Range-capable, so seeking works.
    pub fn stream_url(&self, torrent_id: usize, file_idx: usize) -> String {
        format!("{}/torrents/{torrent_id}/stream/{file_idx}", self.base_url)
    }

    /// Pull from the head of a stream until it is both buffered enough and
    /// demonstrably fast enough, or the budget runs out.
    ///
    /// `required_bps` is the release's own bitrate. When it is unknown (no size
    /// from Torrentio) the gate falls back to buffering a fixed slab and
    /// accepting whatever rate that took, since there is nothing to compare
    /// against.
    pub async fn probe(
        &self,
        url: &str,
        required_bps: Option<u64>,
        mut on_progress: impl FnMut(u64, u64),
    ) -> Result<Probe> {
        let target = prebuffer_target(required_bps);
        let needed_rate = required_bps.map(|bps| (bps as f64 * RATE_SAFETY_FACTOR) as u64);

        let mut res = self
            .http
            .get(url)
            .header("Range", "bytes=0-")
            .send()
            .await
            .context("opening the local stream")?;
        anyhow::ensure!(
            res.status().is_success(),
            "local stream server returned HTTP {}",
            res.status()
        );

        let start = Instant::now();
        let mut total: u64 = 0;
        // The rate over the trailing window. Deliberately NOT a high-water mark:
        // a peak would let a swarm that bursts once and then stalls pass the
        // gate, which is precisely the release we are trying to reject.
        let mut rate: u64 = 0;
        // (instant, cumulative bytes) samples for the sliding-window rate.
        let mut samples: Vec<(Instant, u64)> = vec![(start, 0)];
        let mut last_report = start;

        loop {
            let remaining = PROBE_BUDGET.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Ok(Probe::TooSlow {
                    rate_bps: rate,
                    buffered: total,
                });
            }

            // Abandon a hopeless swarm early instead of burning the whole
            // budget on it. Below half the required rate this far in, it is not
            // going to recover, and the next candidate deserves the time.
            if let Some(needed) = needed_rate {
                if start.elapsed() >= EARLY_ABANDON_AFTER && rate * 2 < needed {
                    return Ok(Probe::TooSlow {
                        rate_bps: rate,
                        buffered: total,
                    });
                }
            }

            let chunk = match tokio::time::timeout(remaining, res.chunk()).await {
                // Budget expired mid-read.
                Err(_) => {
                    return Ok(Probe::TooSlow {
                        rate_bps: rate,
                        buffered: total,
                    })
                }
                Ok(Err(e)) => return Err(e).context("reading from the local stream"),
                // End of file: the whole thing is already here.
                Ok(Ok(None)) => {
                    return Ok(Probe::Ready {
                        rate_bps: rate,
                        buffered: total,
                    })
                }
                Ok(Ok(Some(c))) => c,
            };

            total += chunk.len() as u64;
            let now = Instant::now();
            samples.push((now, total));
            samples.retain(|(t, _)| now.duration_since(*t) <= RATE_WINDOW);

            // Rate over the window. Needs two samples spanning real time,
            // otherwise a single fat chunk reads as infinite throughput.
            if let Some((t0, b0)) = samples.first().copied() {
                let secs = now.duration_since(t0).as_secs_f64();
                if secs >= 0.5 {
                    rate = (((total - b0) as f64 * 8.0) / secs) as u64;
                }
            }

            if now.duration_since(last_report) >= Duration::from_millis(400) {
                on_progress(total.min(target), rate);
                last_report = now;
            }

            let buffered_enough = total >= target;
            let fast_enough = match needed_rate {
                Some(needed) => rate >= needed,
                // Unknown bitrate: buffering the slab is the whole test.
                None => true,
            };
            if buffered_enough && fast_enough {
                on_progress(target, rate);
                return Ok(Probe::Ready {
                    rate_bps: rate,
                    buffered: total,
                });
            }
        }
    }
}

/// Smallest useful pre-buffer, for releases whose bitrate is tiny or unknown.
const PREBUFFER_MIN_BYTES: u64 = 8 * 1024 * 1024;

/// How many bytes to buffer before launching mpv.
fn prebuffer_target(required_bps: Option<u64>) -> u64 {
    required_bps
        .map(|bps| bps / 8 * PREBUFFER_SECS)
        // No size from Torrentio: assume a middling 1080p bitrate so the slab is
        // still a sane size rather than unbounded.
        .unwrap_or(8_000_000 / 8 * PREBUFFER_SECS)
        .clamp(PREBUFFER_MIN_BYTES, PREBUFFER_MAX_BYTES)
}

/// Choose which file inside a torrent to play.
///
/// Torrentio's `fileIdx` is authoritative when present — it is how season packs
/// point at one episode. Without it we match `SxxExx` in the filename, and fall
/// back to the largest video file, which is right for single-movie torrents and
/// avoids sample/extras files.
pub fn choose_file(
    files: &[TorrentFile],
    hint: Option<usize>,
    season: Option<u32>,
    episode: Option<u32>,
) -> Result<usize> {
    anyhow::ensure!(!files.is_empty(), "the torrent contains no files");

    if let Some(idx) = hint {
        if files.iter().any(|f| f.idx == idx) {
            return Ok(idx);
        }
    }

    if let (Some(s), Some(e)) = (season, episode) {
        if let Some(f) = files
            .iter()
            .filter(|f| f.is_video())
            .find(|f| matches_episode(&f.name, s, e))
        {
            return Ok(f.idx);
        }
    }

    files
        .iter()
        .filter(|f| f.is_video())
        .max_by_key(|f| f.len)
        .or_else(|| files.iter().max_by_key(|f| f.len))
        .map(|f| f.idx)
        .context("the torrent contains no playable file")
}

/// Whether a filename names a specific episode.
///
/// Covers the three forms release groups actually use: `S02E05`, `2x05`, and a
/// spelled-out `Season 2 Episode 5`. Matching is done on a lowercased copy with
/// separators collapsed, so `S02.E05` and `S02 E05` both hit.
pub fn matches_episode(filename: &str, season: u32, episode: u32) -> bool {
    let lower = filename.to_ascii_lowercase();
    let flat: String = lower
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    let patterns = [
        format!("s{season:02}e{episode:02}"),
        format!("s{season}e{episode}"),
        format!("{season}x{episode:02}"),
        format!("season {season} episode {episode}"),
    ];
    // The compact forms can appear glued to other text, so check the raw
    // lowercase string too, not just the whitespace-separated one.
    patterns
        .iter()
        .any(|p| lower.contains(p.as_str()) || flat.contains(p.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(idx: usize, name: &str, len: u64) -> TorrentFile {
        TorrentFile {
            idx,
            name: name.into(),
            len,
        }
    }

    #[test]
    fn video_detection_ignores_non_video_files() {
        assert!(f(0, "Movie.1080p.mkv", 1).is_video());
        assert!(f(0, "Movie.MP4", 1).is_video());
        assert!(!f(0, "Movie.nfo", 1).is_video());
        assert!(!f(0, "RARBG.txt", 1).is_video());
    }

    #[test]
    fn torrentio_file_index_wins_when_valid() {
        let files = vec![f(0, "a.mkv", 10), f(1, "b.mkv", 999)];
        assert_eq!(choose_file(&files, Some(0), None, None).unwrap(), 0);
    }

    #[test]
    fn invalid_file_index_falls_through_to_largest_video() {
        let files = vec![f(0, "a.mkv", 10), f(1, "b.mkv", 999)];
        assert_eq!(choose_file(&files, Some(7), None, None).unwrap(), 1);
    }

    #[test]
    fn season_pack_resolves_by_episode_name() {
        let files = vec![
            f(0, "Show.S02E04.1080p.mkv", 100),
            f(1, "Show.S02E05.1080p.mkv", 100),
            f(2, "Show.S02E06.1080p.mkv", 100),
        ];
        assert_eq!(choose_file(&files, None, Some(2), Some(5)).unwrap(), 1);
    }

    #[test]
    fn largest_video_beats_a_bigger_non_video() {
        let files = vec![f(0, "extras.iso", 9_999), f(1, "movie.mkv", 500)];
        assert_eq!(choose_file(&files, None, None, None).unwrap(), 1);
    }

    #[test]
    fn sample_files_lose_to_the_feature() {
        let files = vec![
            f(0, "sample/movie-sample.mkv", 50_000_000),
            f(1, "movie.mkv", 8_000_000_000),
        ];
        assert_eq!(choose_file(&files, None, None, None).unwrap(), 1);
    }

    #[test]
    fn episode_matching_covers_common_release_forms() {
        assert!(matches_episode("Show.S02E05.1080p.WEB.mkv", 2, 5));
        assert!(matches_episode("Show 2x05 720p.mkv", 2, 5));
        assert!(matches_episode("Show - Season 2 Episode 5.mkv", 2, 5));
        assert!(matches_episode("Show.s2e5.mkv", 2, 5));
    }

    #[test]
    fn episode_matching_rejects_the_wrong_episode() {
        assert!(!matches_episode("Show.S02E06.1080p.mkv", 2, 5));
        assert!(!matches_episode("Show.S03E05.1080p.mkv", 2, 5));
    }

    #[test]
    fn prebuffer_scales_with_bitrate_but_is_capped() {
        // 10 Mbps for 45s ≈ 56 MB.
        let small = prebuffer_target(Some(10_000_000));
        assert!((50_000_000..60_000_000).contains(&small), "got {small}");
        // An absurd bitrate is clamped rather than making us wait forever.
        assert_eq!(prebuffer_target(Some(500_000_000)), PREBUFFER_MAX_BYTES);
    }

    #[test]
    fn prebuffer_has_a_floor_for_tiny_bitrates() {
        assert_eq!(prebuffer_target(Some(1)), PREBUFFER_MIN_BYTES);
    }

    #[test]
    fn prebuffer_without_a_known_bitrate_is_within_bounds() {
        let t = prebuffer_target(None);
        assert!((PREBUFFER_MIN_BYTES..=PREBUFFER_MAX_BYTES).contains(&t));
    }

    #[test]
    fn empty_torrent_is_an_error_not_a_panic() {
        assert!(choose_file(&[], None, None, None).is_err());
    }
}
