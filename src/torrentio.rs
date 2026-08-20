//! Torrentio lookup.
//!
//! Torrentio is a free, account-less torrent indexer keyed by IMDB id. It
//! returns torrents rather than direct files, which is exactly what we want:
//! `engine` streams them, so nothing has to finish downloading before playback
//! starts.
//!
//! The parsing here mirrors kojev's `app/lib/downloads.ts` so both agree on what
//! a release's quality, size and seeder count are.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::audio::Audio;

const BASE: &str = "https://torrentio.strem.fun";

/// Public trackers appended to every magnet, so a link still resolves peers
/// when Torrentio omits `sources` for a stream.
const FALLBACK_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
];

const MAX_TRACKERS: usize = 20;

#[derive(Deserialize)]
struct StreamsResponse {
    #[serde(default)]
    streams: Vec<RawStream>,
}

#[derive(Deserialize)]
struct RawStream {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    #[serde(rename = "infoHash")]
    info_hash: Option<String>,
    #[serde(default)]
    #[serde(rename = "fileIdx")]
    file_idx: Option<usize>,
    #[serde(default)]
    sources: Option<Vec<String>>,
    #[serde(default)]
    #[serde(rename = "behaviorHints")]
    behavior_hints: Option<BehaviorHints>,
}

#[derive(Deserialize)]
struct BehaviorHints {
    #[serde(default)]
    filename: Option<String>,
}

/// Quality tiers, ordered. `Ord` is derived from declaration order, so higher
/// variants compare greater — that ordering is what `pick` sorts on.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Quality {
    Cam,
    Sd,
    P480,
    P720,
    P1080,
    P2160,
    P4320,
}

impl Quality {
    pub fn label(self) -> &'static str {
        match self {
            Quality::Cam => "CAM",
            Quality::Sd => "SD",
            Quality::P480 => "480p",
            Quality::P720 => "720p",
            Quality::P1080 => "1080p",
            Quality::P2160 => "4K",
            Quality::P4320 => "8K",
        }
    }

    /// Typical sustained bitrate for a web release of this tier, used when a
    /// candidate's size is unknown and its real bitrate cannot be computed.
    /// Deliberately on the low side for each tier: a too-high guess would
    /// filter out releases the probe could have validated.
    pub fn assumed_bps(self) -> u64 {
        match self {
            Quality::Cam | Quality::Sd | Quality::P480 => 2_500_000,
            Quality::P720 => 4_000_000,
            Quality::P1080 => 8_000_000,
            Quality::P2160 => 16_000_000,
            Quality::P4320 => 45_000_000,
        }
    }

    /// Parse a `--quality` ceiling from the command line.
    pub fn parse_cap(s: &str) -> Option<Quality> {
        match s.trim().to_ascii_lowercase().as_str() {
            "8k" | "4320" | "4320p" => Some(Quality::P4320),
            "4k" | "2160" | "2160p" | "uhd" => Some(Quality::P2160),
            "1080" | "1080p" | "fhd" => Some(Quality::P1080),
            "720" | "720p" | "hd" => Some(Quality::P720),
            "480" | "480p" => Some(Quality::P480),
            "sd" => Some(Quality::Sd),
            _ => None,
        }
    }
}

/// Classify a release's quality from its name/title text.
pub fn parse_quality(text: &str) -> Quality {
    let t = text.to_ascii_lowercase();
    if t.contains("4320p") || contains_word(&t, "8k") {
        return Quality::P4320;
    }
    if t.contains("2160p") || contains_word(&t, "4k") || t.contains("uhd") {
        return Quality::P2160;
    }
    if t.contains("1080p") {
        return Quality::P1080;
    }
    if t.contains("720p") {
        return Quality::P720;
    }
    if t.contains("480p") {
        return Quality::P480;
    }
    for w in ["cam", "hdcam", "ts", "telesync", "hdts"] {
        if contains_word(&t, w) {
            return Quality::Cam;
        }
    }
    Quality::Sd
}

/// Whole-word containment, so "ts" does not match "tsunami" and "4k" does not
/// match "24kbps".
fn contains_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| w == word)
}

/// Parse the "💾 X GB" size marker into bytes.
fn parse_size(title: &str) -> Option<u64> {
    let idx = title.find('💾')?;
    let rest = title[idx + '💾'.len_utf8()..].trim_start();
    let num_end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let value: f64 = rest[..num_end].parse().ok()?;
    let unit: String = rest[num_end..]
        .trim_start()
        .chars()
        .take(2)
        .collect::<String>()
        .to_ascii_uppercase();
    let mult: f64 = match unit.as_str() {
        "TB" => 1024f64.powi(4),
        "GB" => 1024f64.powi(3),
        "MB" => 1024f64.powi(2),
        "KB" => 1024.0,
        _ => return None,
    };
    Some((value * mult) as u64)
}

/// Parse the "👤 N" seeder marker.
fn parse_seeders(title: &str) -> Option<u32> {
    let idx = title.find('👤')?;
    let rest = title[idx + '👤'.len_utf8()..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// One candidate release, normalised and ready to hand to the engine.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub quality: Quality,
    pub title: String,
    pub size_bytes: Option<u64>,
    pub seeders: u32,
    /// Which file inside the torrent holds this episode, when Torrentio says.
    /// Season packs rely on this; without it we fall back to matching by name.
    pub file_idx: Option<usize>,
    pub magnet: String,
    /// Audio tracks, from flag emoji and the release name.
    pub audio: Audio,
}

impl Candidate {
    /// Sustained bitrate this release needs, in bits per second, given a
    /// runtime. `None` when the size is unknown — the live probe then decides
    /// on its own.
    pub fn required_bps(&self, runtime_secs: u32) -> Option<u64> {
        let size = self.size_bytes?;
        if runtime_secs == 0 {
            return None;
        }
        Some(size.saturating_mul(8) / runtime_secs as u64)
    }
}

/// The stream id Torrentio expects: `tt123` for a movie, `tt123:S:E` for an
/// episode.
pub fn stream_id(imdb_id: &str, season: Option<u32>, episode: Option<u32>) -> String {
    match (season, episode) {
        (Some(s), Some(e)) => format!("{imdb_id}:{s}:{e}"),
        _ => imdb_id.to_string(),
    }
}

pub struct Torrentio {
    http: reqwest::Client,
}

impl Torrentio {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("dekho/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("building the Torrentio HTTP client")?;
        Ok(Self { http })
    }

    /// Look up releases for a title, or one episode of a show.
    pub async fn candidates(
        &self,
        imdb_id: &str,
        kind: &str,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Result<Vec<Candidate>> {
        let id = stream_id(imdb_id, season, episode);
        let url = format!("{BASE}/stream/{kind}/{id}.json");
        let res = self
            .http
            .get(&url)
            .send()
            .await
            .context("requesting Torrentio")?;
        anyhow::ensure!(
            res.status().is_success(),
            "Torrentio returned HTTP {}",
            res.status()
        );
        let body: StreamsResponse = res.json().await.context("decoding Torrentio response")?;
        Ok(parse_streams(body.streams))
    }
}

fn parse_streams(streams: Vec<RawStream>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = streams
        .into_iter()
        .filter_map(|s| {
            let info_hash = s.info_hash.clone().filter(|h| !h.is_empty())?;
            let title_text = s.title.clone().unwrap_or_default();
            let meta = format!("{}\n{}", s.name.clone().unwrap_or_default(), title_text);
            let display = display_title(&s);
            Some(Candidate {
                quality: parse_quality(&meta),
                size_bytes: parse_size(&title_text),
                seeders: parse_seeders(&title_text).unwrap_or(0),
                file_idx: s.file_idx,
                magnet: build_magnet(&info_hash, &display, s.sources.as_deref()),
                // Read from the whole blob, not just the filename: the flag
                // emoji live on their own line of `title`, away from the name.
                audio: crate::audio::detect(&meta),
                title: display,
            })
        })
        .collect();

    // Best first: quality, then swarm health. `pick` re-ranks against measured
    // throughput, but this is the order it walks.
    out.sort_by(|a, b| {
        b.quality
            .cmp(&a.quality)
            .then_with(|| b.seeders.cmp(&a.seeders))
    });
    out
}

fn display_title(s: &RawStream) -> String {
    if let Some(f) = s
        .behavior_hints
        .as_ref()
        .and_then(|h| h.filename.as_deref())
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        return f.to_string();
    }
    if let Some(line) = s.title.as_deref().and_then(|t| {
        t.lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with(['👤', '💾', '⚙', '🇬']))
    }) {
        return line.to_string();
    }
    s.name
        .as_deref()
        .and_then(|n| {
            n.lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.eq_ignore_ascii_case("torrentio"))
        })
        .unwrap_or("Unknown release")
        .to_string()
}

pub fn build_magnet(info_hash: &str, display_name: &str, sources: Option<&[String]>) -> String {
    let mut trackers: Vec<String> = sources
        .unwrap_or(&[])
        .iter()
        .filter_map(|s| s.strip_prefix("tracker:"))
        .map(str::to_string)
        .collect();
    for t in FALLBACK_TRACKERS {
        if !trackers.iter().any(|x| x == t) {
            trackers.push((*t).to_string());
        }
    }
    trackers.truncate(MAX_TRACKERS);

    let mut magnet = format!(
        "magnet:?xt=urn:btih:{info_hash}&dn={}",
        percent_encode(display_name)
    );
    for t in trackers {
        magnet.push_str("&tr=");
        magnet.push_str(&percent_encode(&t));
    }
    magnet
}

/// Percent-encode a string, keeping only the unreserved set. Hand-rolled to
/// keep the dependency list at what was approved; shared with `tmdb`'s query
/// encoding.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Human-readable byte size, e.g. "1.4 GB".
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut n = bytes as f64;
    let mut i = 0;
    while n >= 1024.0 && i < UNITS.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    if i == 0 || n >= 10.0 {
        format!("{:.0} {}", n, UNITS[i])
    } else {
        format!("{:.1} {}", n, UNITS[i])
    }
}

/// Bits per second as "12.4 Mbps".
pub fn format_bps(bps: u64) -> String {
    format!("{:.1} Mbps", bps as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Audio;

    #[test]
    fn quality_orders_high_to_low() {
        assert!(Quality::P2160 > Quality::P1080);
        assert!(Quality::P1080 > Quality::P720);
        assert!(Quality::Sd > Quality::Cam);
    }

    #[test]
    fn parses_quality_from_release_names() {
        assert_eq!(parse_quality("Movie 2160p UHD Remux"), Quality::P2160);
        assert_eq!(parse_quality("Movie.1080p.WEB-DL"), Quality::P1080);
        assert_eq!(parse_quality("Movie 720p BluRay"), Quality::P720);
        assert_eq!(parse_quality("Movie HDCAM"), Quality::Cam);
        assert_eq!(parse_quality("Movie DVDRip"), Quality::Sd);
    }

    #[test]
    fn quality_word_match_does_not_fire_on_substrings() {
        // "ts" inside "tsunami" must not classify this as a CAM rip.
        assert_eq!(parse_quality("Tsunami.1080p.WEB"), Quality::P1080);
        // "4k" inside "24kbps" must not promote an SD rip to 4K.
        assert_eq!(parse_quality("Concert 24kbps audio"), Quality::Sd);
    }

    #[test]
    fn parses_size_marker() {
        // Torrentio's "GB" is really GiB: 74.43 * 1024^3.
        assert_eq!(
            parse_size("👤 44 💾 74.43 GB ⚙️ 1337x"),
            Some(79_918_603_960)
        );
        assert_eq!(parse_size("👤 10 💾 900 MB"), Some(943_718_400));
        assert_eq!(parse_size("no size here"), None);
    }

    #[test]
    fn parses_seeder_marker() {
        assert_eq!(parse_seeders("👤 44 💾 74.43 GB"), Some(44));
        assert_eq!(parse_seeders("💾 1 GB"), None);
    }

    #[test]
    fn stream_id_switches_on_episode() {
        assert_eq!(stream_id("tt0903747", None, None), "tt0903747");
        assert_eq!(stream_id("tt0903747", Some(2), Some(5)), "tt0903747:2:5");
    }

    #[test]
    fn required_bitrate_is_size_over_runtime() {
        let c = Candidate {
            quality: Quality::P1080,
            title: "x".into(),
            size_bytes: Some(2 * 1024 * 1024 * 1024),
            seeders: 10,
            file_idx: None,
            magnet: String::new(),
            audio: Audio::default(),
        };
        // 2 GiB over 2 hours ≈ 2.39 Mbps.
        let bps = c.required_bps(7200).unwrap();
        assert!((2_300_000..2_500_000).contains(&bps), "got {bps}");
    }

    #[test]
    fn required_bitrate_is_none_without_a_size() {
        let c = Candidate {
            quality: Quality::P1080,
            title: "x".into(),
            size_bytes: None,
            seeders: 10,
            file_idx: None,
            magnet: String::new(),
            audio: Audio::default(),
        };
        assert!(c.required_bps(7200).is_none());
    }

    #[test]
    fn magnet_carries_hash_and_trackers() {
        let m = build_magnet("abc123", "My Release", None);
        assert!(m.starts_with("magnet:?xt=urn:btih:abc123"));
        assert!(m.contains("dn=My%20Release"));
        assert!(m.contains("tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce"));
    }
}
