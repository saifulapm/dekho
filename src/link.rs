//! Memory of what this internet connection can actually deliver.
//!
//! The throughput gate in `engine` measures one swarm at a time, from scratch,
//! every run. On a fast line that is merely redundant; on a slow one it is the
//! failure mode: the first two attempts go to 4K releases the link could never
//! stream, each burns its probe budget, and the run ends on the "play the
//! fastest one anyway" warning path — which is exactly the stutter the gate
//! exists to prevent.
//!
//! So the link gets a memory: a rolling estimate of achievable throughput,
//! persisted next to `history.json` and updated from every probe that watched
//! real network traffic. The estimate rises quickly and falls slowly, because
//! the two directions mean different things — a fast measurement proves the
//! link can do at least that much, while a slow one may only prove the *swarm*
//! was bad.
//!
//! What the estimate is used for:
//! - deriving an effective `--max-bitrate` when none was given, so a slow line
//!   stops shortlisting releases it cannot stream;
//! - ranking swarm health above quality in `pick` when the line is thin;
//! - sizing the pre-buffer in `engine` against the measured headroom.
//!
//! An explicit `--max-bitrate` always wins, and every adaptation is reported —
//! a user must be able to see *why* they got 720p.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Bumped only if the shape changes in a way an older dekho cannot read.
const VERSION: u32 = 1;

/// EWMA weights. Rising fast and falling slowly is deliberate: a high sample
/// is proof the link can do it, a low sample may just be a bad swarm.
const ALPHA_UP: f64 = 0.5;
const ALPHA_DOWN: f64 = 0.2;

/// Sessions measured before the estimate is trusted to cap anything. One
/// unlucky evening must not decide what next week's runs are allowed to try.
const MIN_SAMPLES: u32 = 3;

/// The ceiling keeps this much of the estimate as headroom — the inverse of
/// the probe's 1.25x gate, so a release at the ceiling is one the gate can
/// still be expected to pass.
const CEILING_HEADROOM: f64 = 0.8;

/// Never cap below this. Even a bad link streams SD, and a floor keeps a few
/// catastrophic samples from filtering out everything.
const MIN_CEILING_BPS: u64 = 2_000_000;

/// The built-in ceiling when nothing else applies — the historical
/// `--max-bitrate` default of 40 Mbps. Adaptation can only lower it.
pub const DEFAULT_CEILING_BPS: u64 = 40_000_000;

/// Below this estimate the link counts as slow, and `pick` starts ranking
/// swarm health above quality. Roughly: a link that cannot clear the gate for
/// a typical 4K WEB-DL (~16 Mbps at 1.25x).
pub const SLOW_LINK_BPS: u64 = 20_000_000;

/// Where the link estimate lives, next to `history.json`.
pub fn path() -> PathBuf {
    crate::xdg::state_home().join("dekho").join("link.json")
}

/// The persisted estimate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Link {
    pub version: u32,
    /// Achievable throughput in bits per second, exponentially smoothed.
    pub ewma_bps: u64,
    /// How many sessions have contributed. Confidence, not history.
    pub samples: u32,
    /// Unix seconds of the last update.
    pub updated: u64,
}

impl Link {
    pub fn empty() -> Self {
        Self {
            version: VERSION,
            ewma_bps: 0,
            samples: 0,
            updated: 0,
        }
    }

    /// Read the estimate. Anything unreadable is an empty one — the next
    /// session measures and rewrites it.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(Self::empty)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("the link path has no parent directory"))?;
        std::fs::create_dir_all(dir)?;
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Fold one measured sample in.
    pub fn observe(&mut self, sample_bps: u64, now: u64) {
        if sample_bps == 0 {
            return;
        }
        if self.samples == 0 {
            self.ewma_bps = sample_bps;
        } else {
            let alpha = if sample_bps > self.ewma_bps {
                ALPHA_UP
            } else {
                ALPHA_DOWN
            };
            let est = self.ewma_bps as f64;
            self.ewma_bps = (est + alpha * (sample_bps as f64 - est)) as u64;
        }
        self.samples = self.samples.saturating_add(1);
        self.updated = now;
    }

    /// The estimate, once there is enough evidence to act on it.
    pub fn trusted_estimate(&self) -> Option<u64> {
        (self.samples >= MIN_SAMPLES && self.ewma_bps > 0).then_some(self.ewma_bps)
    }
}

/// Where the effective bitrate ceiling came from, so the adaptation is
/// visible rather than silent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ceiling {
    /// An explicit `--max-bitrate`, which always wins.
    Flag(u64),
    /// Derived from the remembered link throughput.
    Adapted {
        bps: u64,
        link_bps: u64,
        samples: u32,
    },
    /// No flag and not enough evidence yet: the historical default.
    Default(u64),
}

impl Ceiling {
    pub fn bps(&self) -> u64 {
        match *self {
            Ceiling::Flag(b) | Ceiling::Default(b) => b,
            Ceiling::Adapted { bps, .. } => bps,
        }
    }

    /// The `adaptive` event every playback run emits, so a caller can always
    /// see which ceiling applied and why.
    pub fn json(&self) -> Value {
        match *self {
            Ceiling::Flag(bps) => json!({
                "event": "adaptive", "source": "flag", "max_bitrate_bps": bps,
            }),
            Ceiling::Default(bps) => json!({
                "event": "adaptive", "source": "default", "max_bitrate_bps": bps,
            }),
            Ceiling::Adapted {
                bps,
                link_bps,
                samples,
            } => json!({
                "event": "adaptive", "source": "link", "max_bitrate_bps": bps,
                "link_bps": link_bps, "samples": samples,
            }),
        }
    }
}

/// The bitrate ceiling this run should filter with.
///
/// An explicit flag always wins. Otherwise a trusted estimate caps releases at
/// what the link has actually shown it can carry (with headroom) — never
/// *above* the old default, so a fast line behaves exactly as before.
pub fn effective_ceiling(flag_bps: Option<u64>, link: &Link) -> Ceiling {
    if let Some(bps) = flag_bps {
        return Ceiling::Flag(bps);
    }
    match link.trusted_estimate() {
        Some(est) => {
            let derived = ((est as f64 * CEILING_HEADROOM) as u64).max(MIN_CEILING_BPS);
            if derived >= DEFAULT_CEILING_BPS {
                Ceiling::Default(DEFAULT_CEILING_BPS)
            } else {
                Ceiling::Adapted {
                    bps: derived,
                    link_bps: est,
                    samples: link.samples,
                }
            }
        }
        None => Ceiling::Default(DEFAULT_CEILING_BPS),
    }
}

/// The estimate as shared state: probes report in from the resolution path,
/// and each report is persisted immediately so a killed run still remembers.
pub struct LinkStore {
    path: PathBuf,
    inner: Mutex<Link>,
}

impl LinkStore {
    pub fn open(path: PathBuf) -> Self {
        let link = Link::load(&path);
        Self {
            path,
            inner: Mutex::new(link),
        }
    }

    pub fn snapshot(&self) -> Link {
        self.inner
            .lock()
            .map(|l| l.clone())
            .unwrap_or_else(|_| Link::empty())
    }

    /// Fold a measured sample in and persist. Best-effort: an estimate is not
    /// worth failing playback for.
    pub fn observe(&self, sample_bps: u64) {
        let Ok(mut link) = self.inner.lock() else {
            return;
        };
        link.observe(sample_bps, crate::history::now());
        let _ = link.save(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_sample_is_taken_verbatim() {
        let mut l = Link::empty();
        l.observe(10_000_000, 100);
        assert_eq!(l.ewma_bps, 10_000_000);
        assert_eq!(l.samples, 1);
        assert_eq!(l.updated, 100);
    }

    #[test]
    fn the_estimate_rises_fast_and_falls_slow() {
        let mut l = Link::empty();
        l.observe(10_000_000, 100);
        // A faster sample moves halfway up…
        l.observe(20_000_000, 200);
        assert_eq!(l.ewma_bps, 15_000_000);
        // …a slower one only a fifth of the way down: it may have been the
        // swarm's fault, not the link's.
        l.observe(5_000_000, 300);
        assert_eq!(l.ewma_bps, 13_000_000);
    }

    #[test]
    fn a_zero_sample_is_not_a_measurement() {
        let mut l = Link::empty();
        l.observe(0, 100);
        assert_eq!(l.samples, 0);
    }

    #[test]
    fn too_few_samples_are_not_trusted() {
        let mut l = Link::empty();
        l.observe(6_000_000, 1);
        l.observe(6_000_000, 2);
        assert_eq!(l.trusted_estimate(), None, "two sessions prove nothing");
        l.observe(6_000_000, 3);
        assert_eq!(l.trusted_estimate(), Some(6_000_000));
    }

    fn trusted(bps: u64) -> Link {
        Link {
            version: VERSION,
            ewma_bps: bps,
            samples: MIN_SAMPLES,
            updated: 1,
        }
    }

    #[test]
    fn an_explicit_flag_always_wins() {
        // Even over a link that says the flag is hopeless.
        let c = effective_ceiling(Some(40_000_000), &trusted(3_000_000));
        assert_eq!(c, Ceiling::Flag(40_000_000));
    }

    #[test]
    fn a_slow_link_lowers_the_ceiling_with_headroom() {
        let c = effective_ceiling(None, &trusted(10_000_000));
        match c {
            Ceiling::Adapted { bps, link_bps, .. } => {
                assert_eq!(bps, 8_000_000, "80% of the estimate");
                assert_eq!(link_bps, 10_000_000);
            }
            other => panic!("expected an adapted ceiling, got {other:?}"),
        }
    }

    #[test]
    fn a_fast_link_keeps_the_historical_default() {
        // 100 Mbps measured must not raise the ceiling above the old 40.
        let c = effective_ceiling(None, &trusted(100_000_000));
        assert_eq!(c, Ceiling::Default(DEFAULT_CEILING_BPS));
    }

    #[test]
    fn an_untrusted_link_keeps_the_historical_default() {
        let c = effective_ceiling(None, &Link::empty());
        assert_eq!(c, Ceiling::Default(DEFAULT_CEILING_BPS));
    }

    #[test]
    fn the_ceiling_never_drops_below_the_floor() {
        // A link that measured 1 Mbps still gets to try SD.
        let c = effective_ceiling(None, &trusted(1_000_000));
        assert_eq!(c.bps(), MIN_CEILING_BPS);
    }

    #[test]
    fn a_missing_or_corrupt_store_reads_as_empty() {
        let l = Link::load(Path::new("/nonexistent/dekho/link.json"));
        assert_eq!(l.samples, 0);
    }
}
