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

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use librqbit::api::Api;
use librqbit::http_api::{HttpApi, HttpApiOptions};
use librqbit::{AddTorrent, AddTorrentOptions, ManagedTorrent, Session};
use librqbit_dualstack_sockets::{BindOpts, TcpListener as DualstackTcpListener};

/// How much headroom over the release's own bitrate the swarm must show before
/// we commit. 1.25x absorbs the normal sawtooth of peer churn without being so
/// strict that good releases get rejected.
const RATE_SAFETY_FACTOR: f64 = 1.25;

/// Seconds of video buffered before mpv is launched, when the swarm has
/// comfortable headroom. mpv keeps buffering after this, so it is a floor on
/// the head start, not the total cushion.
const PREBUFFER_SECS: u64 = 45;

/// Seconds buffered when a release only just fits the link. One long wait up
/// front beats repeated rebuffering — the same reasoning as mpv's
/// `--cache-pause-wait`, applied before mpv even starts.
const PREBUFFER_MAX_SECS: u64 = 240;

/// Headroom (link estimate ÷ release bitrate) at and above which the base
/// pre-buffer is enough. Below it the buffer grows toward the maximum.
const COMFORTABLE_HEADROOM: f64 = 2.0;

/// Hard ceiling on the pre-buffer, so a 4K remux does not make us wait minutes.
const PREBUFFER_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// How long a candidate gets to prove its *rate*. Peer discovery over DHT
/// and trackers is not instant, so this has to be patient — but the probe exits
/// the moment the gate is cleared, so a healthy swarm costs only a second or
/// two. When the pre-buffer was scaled up for thin headroom, a release whose
/// rate has already cleared the gate gets extra time to fill the slab (see
/// `probe_budget`); one that has not is still cut off here.
const PROBE_BUDGET: Duration = Duration::from_secs(45);

/// Ceiling on the extended slab-filling budget, however thin the headroom.
const PROBE_BUDGET_MAX: Duration = Duration::from_secs(360);

/// How long a candidate gets before a clearly-hopeless rate ends it early.
/// Long enough for DHT and tracker announces to have produced peers.
const EARLY_ABANDON_AFTER: Duration = Duration::from_secs(12);

/// Minimum time spent watching network throughput before a release can be
/// declared playable, however much of it is already cached on disk.
const MIN_PROBE_SECS: Duration = Duration::from_secs(6);

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
    /// Kept so the probe can read peer counts. Without them, a stalled probe
    /// is indistinguishable from a swarm that simply has not connected yet,
    /// and the two need opposite responses.
    pub handle: Arc<ManagedTorrent>,
}

/// One message from the background stream reader.
enum Read {
    Bytes(u64),
    Eof,
    Failed(String),
}

/// A live reading during the pre-buffer, for the caller's progress display.
#[derive(Clone, Copy, Debug)]
pub struct ProbeStatus {
    pub buffered: u64,
    pub rate_bps: u64,
    /// Peers we are actually connected to.
    pub live_peers: u32,
    /// Peers we have heard about from trackers and DHT.
    pub seen_peers: u32,
}

/// What the probe learned about a candidate.
#[derive(Debug)]
pub enum Probe {
    /// Cleared the gate.
    Ready {
        rate_bps: u64,
        buffered: u64,
        /// The whole slab was already on disk, so nothing was measured and
        /// playback can start instantly. `rate_bps` is 0 in that case.
        from_cache: bool,
    },
    /// Could not sustain the required rate within the budget.
    TooSlow {
        rate_bps: u64,
        buffered: u64,
        live_peers: u32,
        seen_peers: u32,
    },
}

pub struct Engine {
    session: Arc<Session>,
    base_url: String,
    http: reqwest::Client,
    download_dir: PathBuf,
    /// Top-level cache-entry names each live torrent writes into, so the
    /// pruner can be told what must survive. Mirrored to a lock file in the
    /// download dir for the benefit of *other* dekho processes.
    active: Mutex<HashMap<usize, HashSet<String>>>,
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

        let session = Session::new(download_dir.clone())
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
            download_dir,
            active: Mutex::new(HashMap::new()),
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

        let added = Added {
            id: handle.id(),
            files,
            handle,
        };
        self.mark_active(&added);
        Ok(added)
    }

    /// Record which cache entries a torrent writes into, and mirror the whole
    /// set to a lock file so another dekho's pruner leaves them alone.
    fn mark_active(&self, added: &Added) {
        let names: HashSet<String> = added
            .files
            .iter()
            .filter_map(|f| crate::cache::top_component(&f.name))
            .collect();
        if let Ok(mut active) = self.active.lock() {
            active.insert(added.id, names);
            let union: HashSet<String> = active.values().flatten().cloned().collect();
            crate::cache::write_active(&self.download_dir, &union);
        }
    }

    /// Cache entries any torrent in this session is using. The pruner must
    /// never touch these.
    pub fn active_names(&self) -> HashSet<String> {
        self.active
            .lock()
            .map(|a| a.values().flatten().cloned().collect())
            .unwrap_or_default()
    }

    /// Bytes of one file already acquired from the swarm and verified.
    /// Public so the caller can tell when the episode on screen is fully on
    /// disk — the moment queueing the next one stops costing bandwidth.
    pub fn have_bytes(handle: &Arc<ManagedTorrent>, file_idx: usize) -> u64 {
        handle
            .stats()
            .file_progress
            .get(file_idx)
            .copied()
            .unwrap_or(0)
    }

    /// Current peer counts: (connected, known-of).
    fn peers(handle: &Arc<ManagedTorrent>) -> (u32, u32) {
        match handle.stats().live {
            Some(live) => {
                let p = live.snapshot.peer_stats;
                (p.live, p.seen)
            }
            None => (0, 0),
        }
    }

    /// Restrict the torrent to the one file being played.
    ///
    /// Matters for season packs: without it, librqbit fetches every episode in
    /// the pack at once, and the bandwidth that should be going to the episode
    /// on screen is spread across twelve others.
    pub async fn only_file(&self, handle: &Arc<ManagedTorrent>, file_idx: usize) {
        let only = std::collections::HashSet::from([file_idx]);
        let _ = self.session.update_only_files(handle, &only).await;
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
    ///
    /// `link_bps` is the remembered link estimate, when trusted: the thinner
    /// the headroom between it and the release's bitrate, the bigger the slab
    /// buffered before mpv is allowed to start.
    pub async fn probe(
        &self,
        added: &Added,
        file_idx: usize,
        url: &str,
        required_bps: Option<u64>,
        link_bps: Option<u64>,
        mut on_progress: impl FnMut(ProbeStatus),
    ) -> Result<Probe> {
        let target = prebuffer_target(required_bps, link_bps);
        let needed_rate = required_bps.map(|bps| (bps as f64 * RATE_SAFETY_FACTOR) as u64);
        let budget = probe_budget(target, required_bps);

        // Already on disk in full — nothing left to measure, and re-watching
        // should be instant.
        let file_len = added
            .files
            .iter()
            .find(|f| f.idx == file_idx)
            .map(|f| f.len)
            .unwrap_or(0);
        if file_len > 0 && Self::have_bytes(&added.handle, file_idx) >= file_len {
            return Ok(Probe::Ready {
                rate_bps: 0,
                buffered: file_len,
                from_cache: true,
            });
        }

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

        // Read on a separate task and report byte counts over a channel.
        //
        // Reading inline with a timeout does not work: a stalled read blocks for
        // the whole remaining budget, so the rate is never recomputed, progress
        // never renders, and the early-abandon check never re-runs. A stall then
        // reports as "0 bytes at 0 Mbps" — no measurement at all — instead of as
        // the low throughput it is. Decoupling lets the loop below tick on its
        // own clock whether or not data is arriving.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Read>(64);
        tokio::spawn(async move {
            loop {
                let msg = match res.chunk().await {
                    Ok(Some(c)) => Read::Bytes(c.len() as u64),
                    Ok(None) => Read::Eof,
                    Err(e) => Read::Failed(e.to_string()),
                };
                let done = !matches!(msg, Read::Bytes(_));
                // A send error means the probe finished and dropped the
                // receiver; stop reading so the torrent stops being pulled.
                if tx.send(msg).await.is_err() || done {
                    break;
                }
            }
        });

        let start = Instant::now();
        let mut total: u64 = 0;
        // The rate over the trailing window. Deliberately NOT a high-water mark:
        // a peak would let a swarm that bursts once and then stalls pass the
        // gate, which is precisely the release we are trying to reject.
        let mut rate: u64 = 0;

        // What mpv actually consumes is SEQUENTIAL bytes, so that is what the
        // rate has to measure. Bytes acquired from the swarm are not the same
        // thing — pieces arrive out of order, so swarm progress can run at
        // 6.7 Mbps while the in-order stream trickles at 1.2, and gating on the
        // former promises a smoothness the player will not see.
        //
        // The complication is cache. A previous run can leave part of the file
        // on disk; the reader drains that at 3 Gbps, which says nothing about
        // what happens once it runs out. So measurement does not begin until
        // the read position passes everything that was already downloaded.
        let cached_at_start = Self::have_bytes(&added.handle, file_idx);
        // When measurement began, and the read position at that moment.
        let mut measuring: Option<(Instant, u64)> = None;
        let mut samples: VecDeque<(Instant, u64)> = VecDeque::new();
        let mut last_report = start;

        // Average sequential throughput since measurement began. The windowed
        // `rate` is what the gate reacts to, because it responds quickly; this
        // is what the verdict is reported with, because a bursty swarm can show
        // 14 Mbps in the final window while having delivered 3 MB in 45s. Only
        // the average explains why such a release was rejected.
        let average = |m: Option<(Instant, u64)>, total: u64| -> u64 {
            let Some((began, from_bytes)) = m else {
                return 0;
            };
            let secs = began.elapsed().as_secs_f64();
            if secs < 0.5 {
                return 0;
            }
            ((total.saturating_sub(from_bytes) as f64 * 8.0) / secs) as u64
        };
        let mut ticker = tokio::time::interval(Duration::from_millis(250));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let (live_peers, seen_peers) = Self::peers(&added.handle);

            // Two deadlines. The base budget is for proving the rate — a
            // release that has not cleared the gate by then never will. The
            // extended budget only applies while the rate IS cleared and the
            // scaled-up slab is still filling: that waiting is the point of
            // scaling it, not a stall.
            let elapsed = start.elapsed();
            let rate_cleared = needed_rate.map(|needed| rate >= needed).unwrap_or(false);
            if elapsed >= budget || (elapsed >= PROBE_BUDGET && !rate_cleared) {
                return Ok(Probe::TooSlow {
                    rate_bps: average(measuring, total),
                    buffered: total,
                    live_peers,
                    seen_peers,
                });
            }

            // Abandon a hopeless swarm early instead of burning the whole
            // budget on it. Gated on having actually connected to someone:
            // "no peers yet" and "connected but slow" look identical from the
            // byte count alone and need opposite responses — the first deserves
            // more time, the second deserves none.
            if let (Some(needed), Some((began, _))) = (needed_rate, measuring) {
                if live_peers > 0
                    && began.elapsed() >= EARLY_ABANDON_AFTER
                    && rate.saturating_mul(2) < needed
                {
                    return Ok(Probe::TooSlow {
                        rate_bps: average(measuring, total),
                        buffered: total,
                        live_peers,
                        seen_peers,
                    });
                }
            }

            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(Read::Bytes(n)) => total += n,
                    Some(Read::Failed(e)) => {
                        anyhow::bail!("reading from the local stream: {e}")
                    }
                    // End of file: the whole thing is already here.
                    Some(Read::Eof) | None => {
                        return Ok(Probe::Ready {
                            rate_bps: average(measuring, total),
                            buffered: total,
                            from_cache: measuring.is_none(),
                        })
                    }
                },
                _ = ticker.tick() => {}
            }

            // Everything read so far was already on disk (the read position has
            // not passed what a previous run downloaded) and it covers the whole
            // slab: skip the measurement and play now. Re-watching and resuming
            // must not wait six seconds to be told what the disk already proves.
            // The swarm keeps fetching ahead once mpv's stream opens.
            if total >= target && total <= cached_at_start {
                on_progress(ProbeStatus {
                    buffered: target,
                    rate_bps: 0,
                    live_peers,
                    seen_peers,
                });
                return Ok(Probe::Ready {
                    rate_bps: 0,
                    buffered: total,
                    from_cache: true,
                });
            }

            let now = Instant::now();

            // Still draining data that was on disk before we started; nothing
            // measured here would describe the swarm.
            if total <= cached_at_start {
                measuring = None;
                samples.clear();
            } else {
                let (began, _) = *measuring.get_or_insert((now, total));
                if samples.is_empty() {
                    samples.push_back((began, total));
                }
                samples.push_back((now, total));

                // Drop samples that have aged out, but ALWAYS keep one older
                // than the window as the anchor. Dropping every stale sample
                // would leave a single just-pushed entry spanning ~0s, and the
                // stall this measurement exists to catch would read as no
                // measurement at all rather than as low throughput.
                while samples.len() >= 2 && now.duration_since(samples[1].0) > RATE_WINDOW {
                    samples.pop_front();
                }

                // Rate over the window. Needs two samples spanning real time,
                // otherwise a single fat chunk reads as infinite throughput.
                let (t0, b0) = samples[0];
                let secs = now.duration_since(t0).as_secs_f64();
                if secs >= 0.5 {
                    rate = ((total.saturating_sub(b0) as f64 * 8.0) / secs) as u64;
                }
            }

            if now.duration_since(last_report) >= Duration::from_millis(400) {
                on_progress(ProbeStatus {
                    buffered: total.min(target),
                    rate_bps: rate,
                    live_peers,
                    seen_peers,
                });
                last_report = now;
            }

            let buffered_enough = total >= target;
            let fast_enough = match needed_rate {
                Some(needed) => rate >= needed,
                // Unknown bitrate: buffering the slab is the whole test.
                None => true,
            };
            // Never conclude before the swarm has been watched for a while.
            // Without this floor, a cached head satisfies `buffered_enough`
            // instantly and the release is declared fast on no evidence at all.
            let measured_enough = measuring
                .map(|(t, _)| t.elapsed() >= MIN_PROBE_SECS)
                .unwrap_or(false);
            if buffered_enough && fast_enough && measured_enough {
                on_progress(ProbeStatus {
                    buffered: target,
                    rate_bps: rate,
                    live_peers,
                    seen_peers,
                });
                return Ok(Probe::Ready {
                    rate_bps: average(measuring, total),
                    buffered: total,
                    from_cache: false,
                });
            }
        }
    }

    /// Drop a torrent we decided not to play, keeping its pieces on disk.
    ///
    /// Abandoned candidates otherwise keep downloading in the background and
    /// compete for bandwidth with the release actually being watched — which
    /// would undo the whole point of choosing carefully.
    pub async fn forget(&self, id: usize) {
        let _ = self.session.delete(id.into(), false).await;
        if let Ok(mut active) = self.active.lock() {
            active.remove(&id);
            let union: HashSet<String> = active.values().flatten().cloned().collect();
            crate::cache::write_active(&self.download_dir, &union);
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Best-effort: a killed process leaves the file, and the pruner
        // ignores locks whose pid is gone.
        crate::cache::remove_active(&self.download_dir);
    }
}

/// Smallest useful pre-buffer, for releases whose bitrate is tiny or unknown.
const PREBUFFER_MIN_BYTES: u64 = 8 * 1024 * 1024;

/// Seconds of video to buffer, scaled against the headroom between what the
/// link is remembered to deliver and what the release needs.
///
/// Comfortable headroom (2x or more) gets the base 45 s. As headroom thins
/// toward the 1.25x gate the buffer grows linearly toward 240 s: a release
/// that only just fits WILL fall behind during peer churn, and the choice is
/// between absorbing that in a big head start or pausing mid-scene. Without a
/// trusted link estimate there is nothing to scale against, so the base holds.
fn prebuffer_secs(required_bps: Option<u64>, link_bps: Option<u64>) -> u64 {
    let (Some(need), Some(link)) = (required_bps, link_bps) else {
        return PREBUFFER_SECS;
    };
    if need == 0 {
        return PREBUFFER_SECS;
    }
    let headroom = link as f64 / need as f64;
    if headroom >= COMFORTABLE_HEADROOM {
        return PREBUFFER_SECS;
    }
    if headroom <= RATE_SAFETY_FACTOR {
        return PREBUFFER_MAX_SECS;
    }
    let thin = (COMFORTABLE_HEADROOM - headroom) / (COMFORTABLE_HEADROOM - RATE_SAFETY_FACTOR);
    (PREBUFFER_SECS as f64 + thin * (PREBUFFER_MAX_SECS - PREBUFFER_SECS) as f64) as u64
}

/// How many bytes to buffer before launching mpv.
fn prebuffer_target(required_bps: Option<u64>, link_bps: Option<u64>) -> u64 {
    let secs = prebuffer_secs(required_bps, link_bps);
    required_bps
        .map(|bps| bps / 8 * secs)
        // No size from Torrentio: assume a middling 1080p bitrate so the slab is
        // still a sane size rather than unbounded.
        .unwrap_or(8_000_000 / 8 * PREBUFFER_SECS)
        .clamp(PREBUFFER_MIN_BYTES, PREBUFFER_MAX_BYTES)
}

/// How long the probe may run in total.
///
/// The base budget is enough to prove a rate. When the slab was scaled up for
/// thin headroom, filling it at roughly the gate rate takes longer than the
/// base — so the budget covers fetching the whole target at the gate rate with
/// half again to spare, capped. The base-budget rate check in the probe loop
/// still cuts off a release that never clears the gate.
fn probe_budget(target_bytes: u64, required_bps: Option<u64>) -> Duration {
    let Some(bps) = required_bps.filter(|b| *b > 0) else {
        return PROBE_BUDGET;
    };
    let gate = bps as f64 * RATE_SAFETY_FACTOR;
    let fetch_secs = target_bytes as f64 * 8.0 / gate;
    let budget = Duration::from_secs_f64((fetch_secs * 1.5).max(PROBE_BUDGET.as_secs_f64()));
    budget.min(PROBE_BUDGET_MAX)
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
        let small = prebuffer_target(Some(10_000_000), None);
        assert!((50_000_000..60_000_000).contains(&small), "got {small}");
        // An absurd bitrate is clamped rather than making us wait forever.
        assert_eq!(prebuffer_target(Some(500_000_000), None), PREBUFFER_MAX_BYTES);
    }

    #[test]
    fn prebuffer_has_a_floor_for_tiny_bitrates() {
        assert_eq!(prebuffer_target(Some(1), None), PREBUFFER_MIN_BYTES);
    }

    #[test]
    fn prebuffer_without_a_known_bitrate_is_within_bounds() {
        let t = prebuffer_target(None, None);
        assert!((PREBUFFER_MIN_BYTES..=PREBUFFER_MAX_BYTES).contains(&t));
    }

    #[test]
    fn comfortable_headroom_keeps_the_base_prebuffer() {
        // 5 Mbps release on a 20 Mbps link: 4x headroom, no need to wait.
        assert_eq!(prebuffer_secs(Some(5_000_000), Some(20_000_000)), 45);
    }

    #[test]
    fn thin_headroom_grows_the_prebuffer() {
        // 8 Mbps release on a 10 Mbps link: 1.25x headroom, exactly the gate —
        // the buffer does the absorbing that the rate cannot.
        assert_eq!(
            prebuffer_secs(Some(8_000_000), Some(10_000_000)),
            PREBUFFER_MAX_SECS
        );
        // Between the anchors it scales, monotonically.
        let mid = prebuffer_secs(Some(6_000_000), Some(10_000_000));
        assert!((45..PREBUFFER_MAX_SECS).contains(&mid), "got {mid}");
    }

    #[test]
    fn no_link_estimate_means_no_scaling() {
        assert_eq!(prebuffer_secs(Some(8_000_000), None), 45);
        assert_eq!(prebuffer_secs(None, Some(10_000_000)), 45);
    }

    #[test]
    fn probe_budget_grows_with_the_slab_but_is_capped() {
        // A modest slab at a healthy rate: the base budget holds.
        assert_eq!(probe_budget(30_000_000, Some(10_000_000)), PROBE_BUDGET);
        // A 240s slab at 8 Mbps (240 MB) takes ~192s at the gate rate; the
        // budget must cover it with slack.
        let big = probe_budget(240_000_000, Some(8_000_000));
        assert!(big > Duration::from_secs(190), "got {big:?}");
        assert!(big <= PROBE_BUDGET_MAX);
        // Unknown bitrate: nothing to reason with, base budget.
        assert_eq!(probe_budget(999_000_000, None), PROBE_BUDGET);
    }

    #[test]
    fn empty_torrent_is_an_error_not_a_panic() {
        assert!(choose_file(&[], None, None, None).is_err());
    }
}
