//! `dekho play --json` end to end with nothing but loopback: a generated clip
//! seeded by `examples/fake_services.rs`, fake TMDB, Torrentio and apibay on
//! 127.0.0.1, and an mpv whose video and audio outputs are null.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

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

fn make_clip(path: &Path, secs: u32) {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=duration={secs}:size=640x360:rate=25"),
        ])
        .args(["-f", "lavfi", "-i", &format!("sine=frequency=440:duration={secs}")])
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

/// Wait up to `within` for a process to go; true when it has.
fn gone_within(pid: u32, start: u64, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while alive(pid, start) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    !alive(pid, start)
}

/// TCP sockets on this port, from either end, that are still a connection:
/// not the listener itself, and not TIME_WAIT, which the kernel holds after a
/// close whoever owned it.
fn connections_on(port: u16) -> Vec<String> {
    let mut found = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(raw) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in raw.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let port_of = |addr: &str| {
                addr.rsplit(':')
                    .next()
                    .and_then(|p| u16::from_str_radix(p, 16).ok())
            };
            let (local, remote, state) = (port_of(fields[1]), port_of(fields[2]), fields[3]);
            // 0A is LISTEN, 06 is TIME_WAIT.
            if (local == Some(port) || remote == Some(port)) && state != "0A" && state != "06" {
                found.push(line.trim().to_string());
            }
        }
    }
    found
}

/// One dekho run against its own fakes, in its own temp dirs.
struct Rig {
    root: PathBuf,
    dekho: Child,
    dekho_pid: u32,
    events: mpsc::Receiver<String>,
    seen: Vec<Value>,
    seeder_port: u16,
    cleanup: Cleanup,
}

impl Rig {
    /// Start the fakes on a clip of `clip_secs` and `dekho play --json` on
    /// them. A series run plays S1E1 of the fake two-episode season.
    fn start(name: &str, clip_secs: u32, series: bool, fake_args: &[&str], args: &[&str]) -> Rig {
        Rig::start_with(name, |clip| make_clip(clip, clip_secs), series, fake_args, args, &[])
    }

    /// `start` with a clip of the caller's making and extra environment for dekho.
    fn start_with(
        name: &str,
        make: impl FnOnce(&Path),
        series: bool,
        fake_args: &[&str],
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Rig {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("e2e-{name}-{}", std::process::id()));
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
        make(&clip);

        let mut cleanup = Cleanup::default();

        let mut fake = Command::new(fake_services())
            .arg("--clip")
            .arg(&clip)
            .args(series.then_some("--series"))
            .args(fake_args)
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
        let seeder_port = peer.rsplit(':').next().unwrap().parse().unwrap();

        let kind = if series { "tv" } else { "movie" };
        let mut dekho = Command::new(env!("CARGO_BIN_EXE_dekho"))
            .args(["play", "--json", "--id", &tmdb_id, "--kind", kind])
            .args(if series { &["-s", "1", "-e", "1"][..] } else { &[] })
            .args(args)
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
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(File::create(root.join("dekho.log")).unwrap())
            .spawn()
            .expect("starting dekho");
        let dekho_pid = dekho.id();
        cleanup.pids.push(dekho_pid);
        let events = lines_of(&mut dekho);

        Rig {
            root,
            dekho,
            dekho_pid,
            events,
            seen: Vec::new(),
            seeder_port,
            cleanup,
        }
    }

    /// The next NDJSON event that `wanted` accepts, reading for up to `within`.
    fn until(&mut self, within: Duration, wanted: impl Fn(&Value) -> bool) -> Option<Value> {
        let deadline = Instant::now() + within;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(line) = self.events.recv_timeout(left) else {
                return None;
            };
            let event: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            self.seen.push(event.clone());
            if wanted(&event) {
                return Some(event);
            }
        }
        None
    }

    /// Events that arrived since the last read, without waiting.
    fn drain(&mut self) {
        while let Ok(line) = self.events.try_recv() {
            self.seen
                .push(serde_json::from_str(&line).unwrap_or(Value::Null));
        }
    }

    fn said(&self) -> String {
        self.seen
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Wait for `playing` and the mpv it started. Returns mpv's pid and start.
    fn playing(&mut self) -> (u32, u64) {
        let playing = self.until(PLAYING_WITHIN, |e| e["event"] == "playing");
        assert!(
            playing.is_some(),
            "no playing event within {PLAYING_WITHIN:?}; dekho said:\n{}",
            self.said()
        );
        // `playing` is printed before mpv starts.
        let mpv = mpv_child_of(self.dekho_pid, Duration::from_secs(10));
        if let Some((pid, _)) = mpv {
            self.cleanup.pids.push(pid);
        }
        mpv.expect("dekho said playing but started no mpv")
    }

    /// A second client on dekho's mpv IPC socket.
    fn ipc(&self) -> Ipc {
        let socket = self
            .root
            .join("tmp")
            .join(format!("dekho-mpv-{}.sock", self.dekho_pid));
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match UnixStream::connect(&socket) {
                Ok(s) => break s,
                Err(e) if Instant::now() >= deadline => {
                    panic!("connecting to {}: {e}", socket.display())
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        Ipc {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
            next_id: 0,
            events: Vec::new(),
        }
    }

    /// Wait up to `within` for dekho to exit; its exit code when it did.
    fn exited_within(&mut self, within: Duration) -> Option<i32> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.dekho.try_wait().unwrap() {
                return Some(status.code().unwrap_or(-1));
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The watch-history entries dekho saved.
    fn history(&self) -> Vec<Value> {
        let path = self.root.join("state").join("dekho").join("history.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let parsed: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        parsed["items"].as_array().cloned().unwrap_or_default()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// mpv's JSON IPC, from the test's side.
struct Ipc {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
    /// Every event mpv pushed to this client, in order.
    events: Vec<Value>,
}

impl Ipc {
    fn command(&mut self, command: Value) -> Option<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut line = json!({"command": command, "request_id": id}).to_string();
        line.push('\n');
        self.stream.write_all(line.as_bytes()).ok()?;
        loop {
            let mut reply = String::new();
            if self.reader.read_line(&mut reply).ok()? == 0 {
                return None;
            }
            let value: Value = serde_json::from_str(&reply).unwrap_or(Value::Null);
            if value["request_id"] == id {
                return Some(value["data"].clone());
            }
            if value.get("event").is_some() {
                self.events.push(value);
            }
        }
    }

    fn get(&mut self, property: &str) -> Option<Value> {
        self.command(json!(["get_property", property]))
    }
}

#[test]
fn play_reaches_playing_in_a_null_mpv_with_only_loopback() {
    let mut rig = Rig::start("play", 20, false, &[], &[]);
    let (mpv_pid, mpv_start) = rig.playing();
    assert!(
        rig.dekho.try_wait().unwrap().is_none(),
        "dekho exited before it was stopped"
    );

    // SIGKILL runs no destructor in dekho, so only the kernel can take mpv
    // down with it.
    rig.dekho.kill().unwrap();
    rig.dekho.wait().unwrap();
    assert!(
        gone_within(mpv_pid, mpv_start, Duration::from_secs(2)),
        "mpv {mpv_pid} outlived a killed dekho by 2 s"
    );
}

#[test]
fn quitting_mpv_mid_episode_ends_the_run_and_keeps_the_position() {
    // 90 s, so the first five seconds are well before the end is in sight.
    let mut rig = Rig::start("quit", 90, true, &[], &["--queue-next", "immediate"]);
    let (mpv_pid, mpv_start) = rig.playing();
    let mut ipc = rig.ipc();

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        rig.drain();
        let count = ipc.get("playlist-count");
        assert_eq!(
            count,
            Some(json!(1)),
            "the next episode reached mpv's playlist early; dekho said:\n{}",
            rig.said()
        );
        let at = ipc.get("time-pos").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let queued = rig.seen.iter().any(|e| e["event"] == "queued");
        if queued && at >= 5.0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no queued event by {at}s of playback; dekho said:\n{}",
            rig.said()
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    ipc.command(json!(["quit"]));
    let quit_at = Instant::now();
    let code = rig.exited_within(Duration::from_secs(3));
    assert_eq!(code, Some(0), "dekho did not exit within 3 s of mpv; said:\n{}", {
        rig.drain();
        rig.said()
    });
    rig.drain();

    let ended = rig
        .seen
        .iter()
        .position(|e| e["event"] == "ended")
        .unwrap_or_else(|| panic!("no ended event; dekho said:\n{}", rig.said()));
    let exit = rig.seen.iter().position(|e| e["event"] == "exit").unwrap();
    assert!(ended < exit, "ended came after exit");
    let ended = &rig.seen[ended];
    assert_eq!(ended["reason"], "quit", "{ended}");
    let position = ended["position"].as_u64().unwrap();
    assert!((4..=7).contains(&position), "{ended}");

    let history = rig.history();
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0]["episode"], 1, "{}", history[0]);
    assert_eq!(history[0]["finished"], false, "{}", history[0]);
    let saved = history[0]["position"].as_u64().unwrap();
    assert!((4..=7).contains(&saved), "{}", history[0]);

    // No seeding, no player, no dekho, ten seconds on.
    std::thread::sleep(Duration::from_secs(10).saturating_sub(quit_at.elapsed()));
    let left = connections_on(rig.seeder_port);
    assert!(left.is_empty(), "still connected to the seeder:\n{}", left.join("\n"));
    assert!(!alive(mpv_pid, mpv_start), "mpv {mpv_pid} is still there");
    assert!(stat(rig.dekho_pid).is_none(), "dekho {} is still there", rig.dekho_pid);
}

#[test]
fn a_dead_mpv_ends_dekho_while_the_next_episode_resolves() {
    let mut rig = Rig::start(
        "dead",
        90,
        true,
        &["--hold-next"],
        &["--queue-next", "immediate"],
    );
    let (mpv_pid, _) = rig.playing();
    let resolving = rig.until(Duration::from_secs(30), |e| {
        e["event"] == "status"
            && e["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("Looking up releases") && t.contains("S01E02"))
    });
    assert!(
        resolving.is_some(),
        "dekho never looked up the next episode; said:\n{}",
        rig.said()
    );

    let _ = Command::new("kill")
        .args(["-KILL", &mpv_pid.to_string()])
        .status();
    let code = rig.exited_within(Duration::from_secs(3));
    assert!(code.is_some(), "dekho outlived its mpv by 3 s; said:\n{}", {
        rig.drain();
        rig.said()
    });
}

#[test]
fn an_episode_played_to_its_end_hands_over_to_the_next() {
    let mut rig = Rig::start("eof", 20, true, &[], &[]);
    rig.playing();
    let mut ipc = rig.ipc();

    let deadline = Instant::now() + Duration::from_secs(60);
    let title = loop {
        if ipc.get("playlist-pos") == Some(json!(1)) {
            break ipc.get("media-title");
        }
        assert!(
            Instant::now() < deadline,
            "the second episode never started; dekho said:\n{}",
            {
                rig.drain();
                rig.said()
            }
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        title
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|t| t.contains("S01E02")),
        "{title:?}"
    );
    let first_end = ipc
        .events
        .iter()
        .find(|e| e["event"] == "end-file")
        .cloned();
    assert_eq!(
        first_end.as_ref().map(|e| e["reason"].clone()),
        Some(json!("eof")),
        "{:?}",
        ipc.events
    );
    // Finishing an episode moves the title's one entry on to the next.
    let history = rig.history();
    assert_eq!(history[0]["episode"], 2, "{history:?}");

    // The season ends with the second episode, and so does the run.
    let code = rig.exited_within(Duration::from_secs(40));
    rig.drain();
    assert_eq!(code, Some(0), "dekho said:\n{}", rig.said());
    let ended = rig.seen.iter().find(|e| e["event"] == "ended").cloned();
    assert_eq!(
        ended.as_ref().map(|e| e["reason"].clone()),
        Some(json!("eof")),
        "{ended:?}"
    );
    let history = rig.history();
    assert_eq!(history[0]["episode"], 2, "{history:?}");
    assert_eq!(history[0]["finished"], true, "{history:?}");
}

/// Uncompressed 640x360 frames, about 8.6 MB a second, so a short clip is a
/// big file.
fn make_raw_clip(path: &Path, secs: u32) {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=duration={secs}:size=640x360:rate=25"),
        ])
        .args(["-c:v", "rawvideo", "-pix_fmt", "yuv420p", "-f", "matroska"])
        .arg(path)
        .status()
        .expect("running ffmpeg");
    assert!(status.success(), "ffmpeg could not make the raw clip");
}

/// Bytes under a directory, the image cache left out.
fn cached_bytes(dir: &Path) -> u64 {
    let Ok(read) = std::fs::read_dir(dir) else {
        return 0;
    };
    read.flatten()
        .filter(|e| e.file_name() != "img")
        .map(|e| {
            let meta = e.metadata().unwrap();
            if meta.is_dir() {
                cached_bytes(&e.path())
            } else {
                meta.len()
            }
        })
        .sum()
}

#[test]
fn a_film_bigger_than_the_cap_leaves_the_cache_within_it() {
    const CAP: u64 = 100_000_000;
    let clip_secs = 18;
    let mut rig = Rig::start_with(
        "cap",
        |clip| {
            make_raw_clip(clip, clip_secs);
            let size = std::fs::metadata(clip).unwrap().len();
            assert!(size >= 150_000_000, "the raw clip is {size} bytes, not 150 MB");
        },
        false,
        &[],
        &["--max-bitrate", "1000"],
        &[("DEKHO_CACHE_MAX_BYTES", "100000000")],
    );
    rig.playing();
    assert!(
        rig.exited_within(Duration::from_secs(60)).is_some(),
        "dekho did not end with the film; it said:\n{}",
        rig.said()
    );
    let cached = cached_bytes(&rig.root.join("cache").join("dekho"));
    assert!(
        cached <= CAP,
        "the cache holds {cached} bytes after the run, over the {CAP}-byte cap"
    );
}
