//! `dekho play --json` end to end with nothing but loopback: a generated clip
//! seeded by `examples/fake_services.rs`, fake TMDB, Torrentio and apibay on
//! 127.0.0.1, and an mpv whose video and audio outputs are null.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

const PLAYING_WITHIN: Duration = Duration::from_secs(60);

/// Kills whatever the test started, also when an assertion fails, so one
/// failed run leaves nothing behind for the next.
#[derive(Default)]
struct Cleanup {
    children: Vec<Child>,
    pids: Vec<u32>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for pid in &self.pids {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// `CARGO_BIN_EXE_*` names only the crate's binaries, so the example is built
/// here and found beside the `dekho` binary.
fn fake_services() -> PathBuf {
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--offline",
            "--quiet",
            "--example",
            "fake_services",
        ])
        .arg("--manifest-path")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .status()
        .expect("running cargo build for fake_services");
    assert!(status.success(), "building fake_services failed");
    Path::new(env!("CARGO_BIN_EXE_dekho"))
        .parent()
        .unwrap()
        .join("examples")
        .join("fake_services")
}

fn make_clip(path: &Path) {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=20:size=640x360:rate=25",
        ])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=20"])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(path)
        .status()
        .expect("running ffmpeg");
    assert!(status.success(), "ffmpeg could not make the clip");
}

/// Read a child's stdout line by line on a thread, so the test can wait on it
/// with a deadline.
fn lines_of(child: &mut Child) -> mpsc::Receiver<String> {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// A process's parent pid, name and start time from `/proc/<pid>/stat`, or
/// None once it is gone.
fn stat(pid: u32) -> Option<(u32, String, u64)> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    let comm = raw[open + 1..close].to_string();
    let rest: Vec<&str> = raw[close + 2..].split(' ').collect();
    // After the name: state, ppid, ... starttime is field 22 of the whole line.
    let ppid = rest.get(1)?.parse().ok()?;
    let start = rest.get(19)?.parse().ok()?;
    Some((ppid, comm, start))
}

/// The mpv that `parent` started, waiting up to `within` for it to appear.
fn mpv_child_of(parent: u32, within: Duration) -> Option<(u32, u64)> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            if let Some((ppid, comm, start)) = stat(pid) {
                if ppid == parent && comm == "mpv" {
                    return Some((pid, start));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Whether the process that had this pid and start time still exists.
fn alive(pid: u32, start: u64) -> bool {
    stat(pid).is_some_and(|(_, _, s)| s == start)
}

#[test]
fn play_reaches_playing_in_a_null_mpv_with_only_loopback() {
    let root =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-play-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = |name: &str| {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    let (home, config, cache, state, data, runtime, tmp, mpv_home, clip_dir) = (
        dir("home"),
        dir("config"),
        dir("cache"),
        dir("state"),
        dir("data"),
        dir("runtime"),
        dir("tmp"),
        dir("mpv"),
        dir("clip"),
    );
    std::fs::write(mpv_home.join("mpv.conf"), "vo=null\nao=null\n").unwrap();
    let clip = clip_dir.join("clip.mkv");
    make_clip(&clip);

    let mut cleanup = Cleanup::default();

    let mut fake = Command::new(fake_services())
        .arg("--clip")
        .arg(&clip)
        .stdout(Stdio::piped())
        .stderr(File::create(root.join("fake.log")).unwrap())
        .spawn()
        .expect("starting fake_services");
    let fake_lines = lines_of(&mut fake);
    cleanup.children.push(fake);
    let hello: Value = serde_json::from_str(
        &fake_lines
            .recv_timeout(Duration::from_secs(30))
            .expect("fake_services printed no address line"),
    )
    .unwrap();
    let base = hello["base"].as_str().unwrap().to_string();
    let peer = hello["peer"].as_str().unwrap().to_string();
    let tmdb_id = hello["tmdb_id"].as_u64().unwrap().to_string();
    assert!(base.starts_with("http://127.0.0.1:"), "{hello}");
    assert!(peer.starts_with("127.0.0.1:"), "{hello}");

    let mut dekho = Command::new(env!("CARGO_BIN_EXE_dekho"))
        .args(["play", "--json", "--id", &tmdb_id, "--kind", "movie"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_CACHE_HOME", &cache)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("TMPDIR", &tmp)
        .env("MPV_HOME", &mpv_home)
        .env("TMDB_API_KEY", "fake-key")
        .env("DEKHO_TMDB_URL", &base)
        .env("DEKHO_TMDB_IMAGE_URL", &base)
        .env("DEKHO_TORRENTIO_URL", &base)
        .env("DEKHO_APIBAY_URL", &base)
        .env("DEKHO_PEERS", &peer)
        .env("DEKHO_NO_DHT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(File::create(root.join("dekho.log")).unwrap())
        .spawn()
        .expect("starting dekho");
    let dekho_pid = dekho.id();
    let events = lines_of(&mut dekho);
    cleanup.children.push(dekho);

    let deadline = Instant::now() + PLAYING_WITHIN;
    let mut seen = Vec::new();
    let mut playing = false;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(line) = events.recv_timeout(left) else {
            break;
        };
        let event: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        seen.push(line);
        if event["event"] == "playing" {
            playing = true;
            break;
        }
    }
    assert!(
        playing,
        "no playing event within {PLAYING_WITHIN:?}; dekho said:\n{}",
        seen.join("\n")
    );

    let mpv = mpv_child_of(dekho_pid, Duration::from_secs(10));
    if let Some((pid, _)) = mpv {
        cleanup.pids.push(pid);
    }
    let (mpv_pid, mpv_start) = mpv.expect("dekho said playing but started no mpv");
    let (_, _, dekho_start) = stat(dekho_pid).expect("dekho exited before it was stopped");

    // Kill both, as a user closing everything would, and let `Cleanup` reap.
    drop(cleanup);

    let gone_by = Instant::now() + Duration::from_secs(5);
    while (alive(dekho_pid, dekho_start) || alive(mpv_pid, mpv_start)) && Instant::now() < gone_by {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !alive(dekho_pid, dekho_start),
        "dekho {dekho_pid} is still there"
    );
    assert!(!alive(mpv_pid, mpv_start), "mpv {mpv_pid} is still there");

    let _ = std::fs::remove_dir_all(&root);
}
