//! mpv launch and control.
//!
//! Two jobs: start mpv with a buffer big enough that a torrent's uneven
//! delivery never reaches the decoder, and hold a JSON IPC connection so the
//! next episode can be appended to the playlist while the current one plays.
//!
//! The cache flags are the second half of the smoothness story (the first is
//! `engine`'s throughput gate). mpv keeps its own readahead on top of whatever
//! the torrent has on disk, and `--cache-pause-initial` makes it fill that
//! before showing a frame — so a brief swarm stall is absorbed silently instead
//! of pausing playback.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Seconds of audio/video mpv keeps buffered ahead of the playhead.
const CACHE_SECS: u32 = 300;

/// Seconds mpv rebuffers before resuming after an underrun. Generous on
/// purpose: one longer pause beats repeated micro-stutters.
const CACHE_PAUSE_WAIT: u32 = 15;

/// Bounds on the demuxer's in-memory buffer. The lower bound keeps a low-bitrate
/// release from getting a uselessly small cache; the upper bound keeps a 4K
/// remux from trying to hold several GB of RAM.
const DEMUXER_MIN_BYTES: u64 = 256 * 1024 * 1024;
const DEMUXER_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Which YouTube player client yt-dlp should impersonate for a trailer.
///
/// Without this, mpv's own ytdl hook picks a client whose media URLs are
/// refused from here, and every trailer dies on an ffmpeg 403 before a frame
/// is drawn. Measured on this machine, 2026-08-19, unauthenticated:
///
/// ```text
/// mpv <youtube url>                       [ffmpeg] https: HTTP error 403   (c=ANDROID_VR)
/// yt-dlp -f 'best[height<=720]' <url>     ERROR: HTTP Error 403: Forbidden
/// player_client=tv | ios | web            403
/// player_client=web_embedded | mweb       plays
/// ```
///
/// YouTube changes which clients it serves without notice, so when trailers
/// start failing again this is the line to re-test: try `mweb` next, and then
/// cookies from a browser profile (`--ytdl-raw-options=cookies-from-browser=…`),
/// which is the fallback the dotfiles' `packages/manifest.toml` records for the
/// same 403 on music downloads.
const YTDL_PLAYER_CLIENT: &str =
    "--ytdl-raw-options=extractor-args=youtube:player_client=web_embedded";

/// What kind of stream mpv is being pointed at.
///
/// The difference is not cosmetic. A torrent stream is served by a local HTTP
/// endpoint whose delivery is as uneven as the swarm behind it, so it is worth
/// several hundred megabytes of readahead and a pause-before-first-frame; a
/// trailer is an ordinary CDN download, where the same flags would only delay
/// the picture.
#[derive(Clone, Copy, Debug)]
pub enum Mode {
    /// A release streamed from the local torrent engine. Buffers hard, holds
    /// an IPC connection for events and the playhead, and keeps the playlist
    /// alive so the next episode can be appended to it.
    Torrent { bitrate_bps: Option<u64> },
    /// A YouTube trailer. mpv's defaults are already right for an HTTP stream,
    /// there is no playlist to keep alive — so mpv exits when the video ends,
    /// rather than sitting idle on a black window — and nothing about it
    /// belongs in watch history.
    Trailer,
}

/// When a series queues the next episode's torrent.
///
/// `Immediate` is the old behaviour: resolve and buffer the next episode the
/// moment the current one starts. On a thin pipe that steals bandwidth from
/// what is on screen for the whole first act, so the default is `Auto`: wait
/// until the current episode is comfortably buffered — its file fully on disk,
/// or playback past the halfway mark, whichever comes first. On a fast link
/// the file completes in minutes and `Auto` behaves like `Immediate`; on a
/// slow one the episode on screen keeps the whole pipe while it needs it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QueueWhen {
    Immediate,
    Auto,
    /// Queue once playback is this fraction of the way through (0–1), or the
    /// file has finished downloading, whichever comes first.
    Fraction(f64),
}

impl QueueWhen {
    /// Parse `immediate`, `auto`, or a percentage like `50%` / `50`.
    pub fn parse(s: &str) -> Option<QueueWhen> {
        let v = s.trim().to_ascii_lowercase();
        match v.as_str() {
            "immediate" | "now" => return Some(QueueWhen::Immediate),
            "auto" => return Some(QueueWhen::Auto),
            _ => {}
        }
        let digits = v.strip_suffix('%').unwrap_or(&v);
        let pct: u32 = digits.parse().ok()?;
        (1..=100).contains(&pct).then(|| QueueWhen::Fraction(pct as f64 / 100.0))
    }

    /// Whether the next episode should be queued now.
    ///
    /// `played` is how far into the current episode playback is (None until
    /// mpv reports a duration); `downloaded` is whether the current episode's
    /// file is fully on disk — at which point queueing costs nothing, whatever
    /// the policy says about fractions.
    pub fn should_queue(self, played: Option<f64>, downloaded: bool) -> bool {
        match self {
            QueueWhen::Immediate => true,
            QueueWhen::Auto => downloaded || played.map(|f| f >= 0.5).unwrap_or(false),
            QueueWhen::Fraction(at) => {
                downloaded || played.map(|f| f >= at).unwrap_or(false)
            }
        }
    }
}

/// An mpv event we care about.
#[derive(Debug, Clone)]
pub enum Event {
    /// A new playlist entry started playing.
    StartFile,
    /// A playlist entry finished. `reason` is mpv's own wording.
    EndFile { reason: String },
    /// mpv ran out of playlist and went idle.
    Idle,
}

#[derive(Deserialize)]
struct RawEvent {
    event: Option<String>,
    reason: Option<String>,
    /// Set on `property-change` only.
    name: Option<String>,
    data: Option<serde_json::Value>,
}

/// Somewhere to report where playback has reached.
///
/// `history::Recorder` is the only implementor; the indirection keeps the store
/// out of the mpv plumbing. mpv volunteers `time-pos` on its own cadence once
/// the property is observed, so nothing here polls — the reader task has to be
/// awake for events anyway.
pub trait ProgressSink: Send + Sync + 'static {
    /// Latest position and length in seconds. Duration is 0 until mpv knows it.
    fn progress(&self, position: u64, duration: u64);
    /// Persist now: a file ended, or playback is over.
    fn flush(&self);
}

pub struct Mpv {
    child: Child,
    /// Only a `Torrent` launch has one; a trailer needs no IPC.
    socket: Option<PathBuf>,
    writer: Option<tokio::net::unix::OwnedWriteHalf>,
    events: mpsc::UnboundedReceiver<Event>,
}

impl Mpv {
    /// Launch mpv on `url` in the given mode.
    ///
    /// `start_secs` resumes mid-title; `sink` receives the playhead as it
    /// moves. `quiet` discards mpv's own terminal output, which belongs to
    /// whoever is watching a terminal — and mpv writes its status line to
    /// *stdout*, so leaving it on would put `AV: 00:00:03 …` in the middle of
    /// a caller's NDJSON. Warnings still reach stderr either way.
    pub async fn launch(
        url: &str,
        title: &str,
        mode: Mode,
        start_secs: Option<u64>,
        sink: Option<Arc<dyn ProgressSink>>,
        quiet: bool,
    ) -> Result<Self> {
        let mut cmd = Command::new("mpv");
        cmd.arg(url)
            .arg(format!("--force-media-title={title}"))
            .stdin(Stdio::null());

        if quiet {
            cmd.stdout(Stdio::null());
        }

        if let Some(secs) = start_secs {
            cmd.arg(format!("--start={secs}"));
        }

        let socket = match mode {
            Mode::Torrent { bitrate_bps } => {
                let socket =
                    std::env::temp_dir().join(format!("dekho-mpv-{}.sock", std::process::id()));
                // A stale socket from a crashed run would make mpv fail to bind.
                let _ = std::fs::remove_file(&socket);
                cmd.arg(format!("--input-ipc-server={}", socket.display()))
                    // --- buffering --------------------------------------------
                    .arg("--cache=yes")
                    .arg(format!("--cache-secs={CACHE_SECS}"))
                    .arg(format!(
                        "--demuxer-max-bytes={}",
                        demuxer_bytes_for(bitrate_bps)
                    ))
                    .arg(format!("--demuxer-readahead-secs={CACHE_SECS}"))
                    .arg("--cache-pause=yes")
                    .arg(format!("--cache-pause-wait={CACHE_PAUSE_WAIT}"))
                    .arg("--cache-pause-initial=yes")
                    // Keep the playlist alive between episodes so appending works.
                    .arg("--idle=yes")
                    .arg("--keep-open=no")
                    // Open on a Hindi track when the file has one; mpv falls
                    // back to the file's default track otherwise, so this is
                    // free for releases with nothing to choose. Global, so
                    // episodes appended to the playlist inherit it.
                    .arg("--alang=hi,hin");
                Some(socket)
            }
            Mode::Trailer => {
                cmd.arg(YTDL_PLAYER_CLIENT);
                None
            }
        };

        let child = cmd
            .spawn()
            .context("launching mpv — is it installed and on PATH?")?;

        let observing = sink.is_some();
        let (writer, events) = match &socket {
            Some(socket) => {
                let (writer, events) = connect_ipc(socket, sink).await?;
                (Some(writer), events)
            }
            // Nothing to talk to: the sender is dropped, so `next_event`
            // reports the end of the stream immediately and `wait` is the
            // only thing a caller can usefully do.
            None => {
                let (_tx, events) = mpsc::unbounded_channel();
                (None, events)
            }
        };

        let mut mpv = Self {
            child,
            socket,
            writer,
            events,
        };

        // Asking once is enough: mpv then pushes a `property-change` whenever
        // the value moves, which is the same stream of positions a polling loop
        // would build and costs nothing between them.
        if observing && mpv.writer.is_some() {
            for (id, property) in [(1, "time-pos"), (2, "duration")] {
                mpv.send(&serde_json::json!({"command": ["observe_property", id, property]}))
                    .await
                    .ok();
            }
        }

        Ok(mpv)
    }

    /// Append a file to mpv's playlist. mpv advances to it automatically at EOF,
    /// which is what makes the episode change seamless.
    ///
    /// `start=none` because `--start` on the command line is the default for
    /// *every* entry, not just the first: resuming an episode 23 minutes in and
    /// letting the season run would otherwise open the next one 23 minutes in
    /// as well.
    ///
    /// `append-play` rather than `append`: identical while something is
    /// playing, but if mpv already ran out of playlist and went idle — which
    /// deferred queueing makes possible when an episode's resolve takes longer
    /// than its credits — the appended entry starts instead of sitting in a
    /// playlist nobody restarts.
    pub async fn append(&mut self, url: &str, title: &str) -> Result<()> {
        let cmd = serde_json::json!({
            "command": [
                "loadfile",
                url,
                "append-play",
                -1,
                format!("force-media-title=%{}%{},start=none", title.len(), title)
            ]
        });
        self.send(&cmd).await
    }

    async fn send(&mut self, value: &serde_json::Value) -> Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            anyhow::bail!("mpv IPC connection is closed");
        };
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        writer
            .write_all(&line)
            .await
            .context("writing to the mpv IPC socket")?;
        writer.flush().await.ok();
        Ok(())
    }

    /// Next event from mpv, or `None` once the connection ends.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// Wait for mpv to exit.
    pub async fn wait(&mut self) -> Result<()> {
        self.child.wait().await.context("waiting for mpv")?;
        Ok(())
    }

    /// Whether mpv has already exited.
    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        if let Some(socket) = &self.socket {
            let _ = std::fs::remove_file(socket);
        }
    }
}

/// Connect to mpv's IPC socket, retrying while mpv gets around to creating it,
/// and spawn a reader that turns event lines into `Event`s.
async fn connect_ipc(
    socket: &Path,
    sink: Option<Arc<dyn ProgressSink>>,
) -> Result<(
    tokio::net::unix::OwnedWriteHalf,
    mpsc::UnboundedReceiver<Event>,
)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match UnixStream::connect(socket).await {
            Ok(s) => break s,
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(e).context("connecting to the mpv IPC socket");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };

    let (read_half, write_half) = stream.into_split();
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let mut lines = BufReader::new(read_half).lines();
        let mut duration: u64 = 0;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(raw) = serde_json::from_str::<RawEvent>(&line) else {
                continue;
            };
            if raw.event.as_deref() == Some("property-change") {
                let Some(sink) = sink.as_ref() else { continue };
                // mpv sends null until a value exists — before a file loads,
                // and for the whole of anything that never reports a duration.
                let Some(value) = raw
                    .data
                    .and_then(|d| d.as_f64())
                    .filter(|v| v.is_finite() && *v >= 0.0)
                else {
                    continue;
                };
                match raw.name.as_deref() {
                    Some("duration") => duration = value as u64,
                    Some("time-pos") => sink.progress(value as u64, duration),
                    _ => {}
                }
                continue;
            }
            let event = match raw.event.as_deref() {
                Some("start-file") => {
                    // The next entry's length is not known yet, and mpv reports
                    // the gap as null rather than as a change — so without this
                    // the first seconds of an episode would be filed against the
                    // previous one's length.
                    duration = 0;
                    Event::StartFile
                }
                Some("end-file") => {
                    // Before the receiver hears about it: whatever mpv plays
                    // next must not have its first seconds recorded against the
                    // entry that just ended.
                    if let Some(sink) = sink.as_ref() {
                        sink.flush();
                    }
                    Event::EndFile {
                        reason: raw.reason.unwrap_or_else(|| "unknown".into()),
                    }
                }
                Some("idle") => Event::Idle,
                _ => continue,
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });

    Ok((write_half, rx))
}

/// Size mpv's demuxer cache from the stream's bitrate, clamped both ways.
fn demuxer_bytes_for(bitrate_bps: Option<u64>) -> u64 {
    let bps = bitrate_bps.unwrap_or(10_000_000);
    let wanted = bps / 8 * CACHE_SECS as u64;
    wanted.clamp(DEMUXER_MIN_BYTES, DEMUXER_MAX_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demuxer_cache_scales_with_bitrate() {
        // 20 Mbps over 300s = 750 MB, inside the clamp.
        let mid = demuxer_bytes_for(Some(20_000_000));
        assert!((700_000_000..800_000_000).contains(&mid), "got {mid}");
    }

    #[test]
    fn demuxer_cache_is_clamped_at_both_ends() {
        assert_eq!(demuxer_bytes_for(Some(1_000)), DEMUXER_MIN_BYTES);
        assert_eq!(demuxer_bytes_for(Some(900_000_000)), DEMUXER_MAX_BYTES);
    }

    #[test]
    fn unknown_bitrate_gets_a_sane_default() {
        let d = demuxer_bytes_for(None);
        assert!((DEMUXER_MIN_BYTES..=DEMUXER_MAX_BYTES).contains(&d));
    }

    #[test]
    fn the_youtube_player_client_reaches_mpv_as_one_argument() {
        // mpv splits --ytdl-raw-options on `,` and each entry on its first
        // `=`, so this must stay comma-free or yt-dlp gets half an option.
        assert!(!YTDL_PLAYER_CLIENT.contains(','));
        assert!(YTDL_PLAYER_CLIENT.starts_with("--ytdl-raw-options="));
        assert!(YTDL_PLAYER_CLIENT.ends_with("player_client=web_embedded"));
    }

    #[test]
    fn queue_when_parses_the_documented_forms() {
        assert_eq!(QueueWhen::parse("immediate"), Some(QueueWhen::Immediate));
        assert_eq!(QueueWhen::parse("auto"), Some(QueueWhen::Auto));
        assert_eq!(QueueWhen::parse("50%"), Some(QueueWhen::Fraction(0.5)));
        assert_eq!(QueueWhen::parse("25"), Some(QueueWhen::Fraction(0.25)));
        assert_eq!(QueueWhen::parse("0%"), None, "0 would mean immediate; say so");
        assert_eq!(QueueWhen::parse("150%"), None);
        assert_eq!(QueueWhen::parse("soon"), None);
    }

    #[test]
    fn immediate_queues_before_anything_is_known() {
        assert!(QueueWhen::Immediate.should_queue(None, false));
    }

    #[test]
    fn auto_waits_for_a_buffered_episode_or_the_halfway_mark() {
        let auto = QueueWhen::Auto;
        assert!(!auto.should_queue(None, false), "nothing known yet: wait");
        assert!(!auto.should_queue(Some(0.2), false), "early on: the pipe is busy");
        assert!(auto.should_queue(Some(0.5), false), "halfway: time to prepare");
        assert!(
            auto.should_queue(Some(0.1), true),
            "fully downloaded: queueing costs nothing"
        );
    }

    #[test]
    fn a_fraction_policy_uses_its_own_threshold() {
        let at75 = QueueWhen::Fraction(0.75);
        assert!(!at75.should_queue(Some(0.5), false));
        assert!(at75.should_queue(Some(0.75), false));
        assert!(at75.should_queue(Some(0.0), true), "download-complete overrides");
    }
}
