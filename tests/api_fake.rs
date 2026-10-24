//! `dekho api` against the fake TMDB in `examples/fake_services.rs`: the names,
//! credit groups and person filters a title, person or browse page draws.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value};

/// A running fake and a scratch home for dekho to run in. Dropping it kills the
/// fake.
struct Rig {
    fake: Child,
    base: String,
    root: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.fake.kill();
        let _ = self.fake.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fake_services() -> PathBuf {
    let status = Command::new(env!("CARGO"))
        .args(["build", "--offline", "--quiet", "--example", "fake_services"])
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

impl Rig {
    fn start(name: &str) -> Rig {
        let root = std::env::temp_dir().join(format!("dekho-api-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // The fake seeds whatever file it is given; none of these tests play it.
        let clip = root.join("clip.bin");
        std::fs::write(&clip, vec![7u8; 4096]).unwrap();

        let mut fake = Command::new(fake_services())
            .arg("--clip")
            .arg(&clip)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("starting fake_services");
        let mut line = String::new();
        BufReader::new(fake.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let hello: Value = serde_json::from_str(&line).expect("fake_services printed no address");
        let base = hello["base"].as_str().unwrap().to_string();
        Rig { fake, base, root }
    }

    /// Run `dekho api <args>` and parse the one JSON object it prints.
    fn api(&self, args: &[&str]) -> Value {
        let dir = |n: &str| {
            let d = self.root.join(n);
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        let out = Command::new(env!("CARGO_BIN_EXE_dekho"))
            .arg("api")
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", dir("home"))
            .env("XDG_CONFIG_HOME", dir("config"))
            .env("XDG_CACHE_HOME", dir("cache"))
            .env("XDG_STATE_HOME", dir("state"))
            .env("XDG_DATA_HOME", dir("data"))
            .env("TMDB_API_KEY", "fake-key")
            .env("DEKHO_TMDB_URL", &self.base)
            .env("DEKHO_TMDB_IMAGE_URL", &self.base)
            .output()
            .expect("running dekho");
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "dekho api {args:?} printed no JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    /// Every request target the fake has answered so far.
    fn requests(&self) -> Vec<String> {
        let addr = self.base.trim_start_matches("http://");
        let mut sock = TcpStream::connect(addr).unwrap();
        write!(sock, "GET /__requests HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let mut reply = String::new();
        sock.read_to_string(&mut reply).unwrap();
        let body = reply.split("\r\n\r\n").nth(1).unwrap();
        serde_json::from_str(body).unwrap()
    }
}

#[test]
fn a_title_names_its_countries_and_original_language() {
    let rig = Rig::start("title");
    let title = rig.api(&["title", "--id", "900003", "--kind", "movie"]);
    assert_eq!(title["countries"], json!(["Sweden", "United Kingdom"]));
    assert_eq!(title["language"], json!("Swedish"));
    assert_eq!(title["language_code"], json!("sv"));
}

#[test]
fn a_person_splits_credits_into_directing_acting_and_writing() {
    let rig = Rig::start("person");
    let person = rig.api(&["person", "--id", "900200"]);
    assert_eq!(person["place_of_birth"], json!("Gothenburg, Sweden"));

    let roles = |group: &str| -> Vec<(String, String)> {
        person["credits"][group]
            .as_array()
            .unwrap_or_else(|| panic!("no {group} group in {person}"))
            .iter()
            .map(|c| {
                (
                    c["title"].as_str().unwrap().to_string(),
                    c["role"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let pair = |t: &str, r: &str| (t.to_string(), r.to_string());
    // Newest first, and no gaffer or assistant director in any group.
    assert_eq!(
        roles("directing"),
        [pair("Her Latest", "Director"), pair("Her Debut", "Director")]
    );
    assert_eq!(roles("writing"), [pair("Her Script", "Screenplay")]);
    assert_eq!(
        roles("acting"),
        [pair("New Role", "Ada"), pair("Old Role", "Nina")]
    );
}

#[test]
fn people_searches_by_name() {
    let rig = Rig::start("people");
    let found = rig.api(&["people", "mara"]);
    assert_eq!(
        found["items"],
        json!([{
            "id": 900200,
            "name": "Mara Vance",
            "known_for_department": "Directing",
            "profile": "/mara.jpg",
        }])
    );
    assert!(
        rig.requests()
            .iter()
            .any(|r| r.starts_with("/search/person?") && r.contains("query=mara")),
        "requests: {:?}",
        rig.requests()
    );
}

#[test]
fn discover_by_person_asks_tmdb_for_with_people_on_movies() {
    let rig = Rig::start("discover-movie");
    let page = rig.api(&["discover", "--kind", "movie", "--person", "900200"]);
    assert_eq!(page["items"][0]["title"], json!("Her Debut"));
    assert!(
        rig.requests()
            .iter()
            .any(|r| r.starts_with("/discover/movie?") && r.contains("with_people=900200")),
        "requests: {:?}",
        rig.requests()
    );
}

#[test]
fn discover_by_person_lists_shows_from_both_cast_and_crew() {
    let rig = Rig::start("discover-tv");
    let page = rig.api(&["discover", "--kind", "tv", "--person", "900200"]);
    let mut titles: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    titles.sort();
    assert_eq!(titles, ["Her Show Directed", "Her Show Role"]);
}

#[test]
fn watched_marks_an_episode_of_a_title_never_played_and_unset_clears_it() {
    let rig = Rig::start("watched");
    let watched = |rig: &Rig| -> Vec<bool> {
        rig.api(&["episodes", "--id", "900002", "-s", "1"])["items"]
            .as_array()
            .unwrap_or_else(|| panic!("no episodes"))
            .iter()
            .map(|e| e["watched"].as_bool().unwrap())
            .collect()
    };
    // Cold, so the season the API cache keeps was written before any mark.
    assert_eq!(watched(&rig), [false, false]);

    let marked = rig.api(&["watched", "--id", "900002", "-s", "1", "-e", "2"]);
    assert_eq!(marked, json!({"watched": true}));
    assert_eq!(watched(&rig), [false, true]);
    // Only marked, never played: nothing to continue.
    assert_eq!(rig.api(&["history"]), json!({"items": []}));

    let unmarked = rig.api(&["watched", "--id", "900002", "-s", "1", "-e", "2", "--unset"]);
    assert_eq!(unmarked, json!({"watched": false}));
    assert_eq!(watched(&rig), [false, false]);
}

#[test]
fn a_discover_hit_names_its_language_and_genres() {
    let rig = Rig::start("discover-hit");
    let page = rig.api(&["discover", "--kind", "movie", "--lang", "sv"]);
    let hit = &page["items"][1];
    assert_eq!(hit["title"], json!("Lilja Hem"));
    assert_eq!(hit["language"], json!("Swedish"));
    assert_eq!(hit["genres"], json!(["Drama"]));
}

#[test]
fn discover_takes_any_two_letter_language_code() {
    let rig = Rig::start("discover-lang");
    let _ = rig.api(&["discover", "--kind", "tv", "--lang", "sv"]);
    assert!(
        rig.requests()
            .iter()
            .any(|r| r.starts_with("/discover/tv?") && r.contains("with_original_language=sv")),
        "requests: {:?}",
        rig.requests()
    );
}
