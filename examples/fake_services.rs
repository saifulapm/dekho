//! Local stand-ins for every host dekho talks to, so a run of dekho can play a
//! title without a packet leaving the machine.
//!
//! `fake_services --clip <file> [--series] [--hold-next] [--upload-bps <n>]`
//! seeds the clip from a librqbit session on 127.0.0.1, at most `n` bytes a
//! second when given, and serves fake TMDB, Torrentio and apibay answers on
//! one 127.0.0.1 port. It prints one JSON line, then serves until killed:
//!
//! `{"base":"http://127.0.0.1:<port>","peer":"127.0.0.1:<port>","tmdb_id":<id>,"kind":"movie"|"tv"}`
//!
//! Point dekho at it with DEKHO_TMDB_URL, DEKHO_TMDB_IMAGE_URL,
//! DEKHO_TORRENTIO_URL and DEKHO_APIBAY_URL set to `base`, DEKHO_PEERS set to
//! `peer`, and DEKHO_NO_DHT=1. The TMDB paths, Torrentio's `/stream/` and
//! apibay's `/q.php` never collide, so one base serves all three.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::PathBuf;

use anyhow::{Context, Result};
use librqbit::limits::LimitsConfig;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    create_torrent, AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session,
    SessionOptions,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MOVIE_ID: u32 = 900001;
const SHOW_ID: u32 = 900002;
const MOVIE_IMDB: &str = "tt9000001";
const SHOW_IMDB: &str = "tt9000002";

/// The release name the fake Torrentio gives the clip. It carries the TMDB
/// title's words, so dekho's title guard keeps it, and a quality tag.
const RELEASE_NAME: &str = "Test.Pattern.2024.720p.WEB.mkv";

/// Seeders the fake Torrentio claims, above dekho's default `--min-seeders` 4.
const SEEDERS: u32 = 25;

const MOVIE: &str = include_str!("../tests/fixtures/fake/movie.json");
const TV: &str = include_str!("../tests/fixtures/fake/tv.json");
const SEASON_1: &str = include_str!("../tests/fixtures/fake/season_1.json");
const SEARCH: &str = include_str!("../tests/fixtures/fake/search.json");
const LANGUAGES: &str = include_str!("../tests/fixtures/fake/languages.json");
const GENRES: &str = include_str!("../tests/fixtures/fake/genres.json");
const PERSON: &str = include_str!("../tests/fixtures/fake/person.json");
// t5 fixtures
const MOVIE_SE: &str = include_str!("../tests/fixtures/fake/movie_se.json");
const COUNTRIES: &str = include_str!("../tests/fixtures/fake/countries.json");
const PERSON_MARA: &str = include_str!("../tests/fixtures/fake/person_mara.json");
const PERSON_MARA_TV: &str = include_str!("../tests/fixtures/fake/person_mara_tv.json");
const SEARCH_PERSON: &str = include_str!("../tests/fixtures/fake/search_person.json");
const DISCOVER_MOVIE: &str = include_str!("../tests/fixtures/fake/discover_movie.json");
const APIBAY_EMPTY: &str = include_str!("../tests/fixtures/fake/apibay_empty.json");

/// TMDB's body for a path it does not know.
const NOT_FOUND: &str = r#"{"success":false,"status_code":34,"status_message":"The resource you requested could not be found."}"#;

struct Args {
    clip: PathBuf,
    series: bool,
    hold_next: bool,
    /// A cap on the seeder's upload in bytes per second, so a probe sees the
    /// clip arrive over seconds rather than in one burst.
    upload_bps: Option<NonZeroU32>,
}

fn args() -> Result<Args> {
    let mut clip = None;
    let mut series = false;
    let mut hold_next = false;
    let mut upload_bps = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--clip" => clip = Some(PathBuf::from(it.next().context("--clip needs a file")?)),
            "--series" => series = true,
            "--hold-next" => hold_next = true,
            "--upload-bps" => {
                upload_bps = Some(
                    it.next()
                        .context("--upload-bps needs a number")?
                        .parse()
                        .context("--upload-bps needs a number above 0")?,
                )
            }
            other => anyhow::bail!(
                "unknown argument {other:?}; usage: fake_services --clip <file> [--series] [--hold-next] [--upload-bps <n>]"
            ),
        }
    }
    Ok(Args {
        clip: clip.context(
            "usage: fake_services --clip <file> [--series] [--hold-next] [--upload-bps <n>]",
        )?,
        series,
        hold_next,
        upload_bps,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = args()?;
    let clip = args
        .clip
        .canonicalize()
        .with_context(|| format!("reading {}", args.clip.display()))?;
    let clip_len = std::fs::metadata(&clip)?.len();

    let (peer, info_hash, _session) = seed(&clip, args.upload_bps).await?;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (tmdb_id, kind) = if args.series {
        (SHOW_ID, "tv")
    } else {
        (MOVIE_ID, "movie")
    };
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "{}",
        json!({"base": base, "peer": peer.to_string(), "tmdb_id": tmdb_id, "kind": kind})
    )?;
    stdout.flush()?;
    drop(stdout);

    let streams = torrentio_streams(&info_hash, clip_len);
    let hold_next = args.hold_next;
    loop {
        let (sock, _) = listener.accept().await?;
        let streams = streams.clone();
        tokio::spawn(async move {
            let _ = serve(sock, &streams, hold_next).await;
        });
    }
}

/// Seed the clip from a session that listens on loopback only and asks no
/// DHT, tracker or LAN for anything. Returns the address to dial it on.
async fn seed(
    clip: &std::path::Path,
    upload_bps: Option<NonZeroU32>,
) -> Result<(SocketAddr, String, std::sync::Arc<Session>)> {
    let torrent = create_torrent(
        clip,
        CreateTorrentOptions::default(),
        &BlockingSpawner::new(1),
    )
    .await
    .context("hashing the clip into a torrent")?;
    let folder = torrent.output_folder.clone();

    let session = Session::new_with_opts(
        folder.clone(),
        SessionOptions {
            dht: None,
            disable_trackers: true,
            disable_local_service_discovery: true,
            persistence: None,
            listen: Some(ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                ipv4_only: true,
                ..Default::default()
            }),
            ratelimits: LimitsConfig {
                upload_bps,
                download_bps: None,
            },
            ..Default::default()
        },
    )
    .await
    .context("starting the seeding session")?;

    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent.as_bytes()?),
            Some(AddTorrentOptions {
                // The clip is already there; overwrite lets librqbit check it
                // and seed it instead of refusing an existing file.
                overwrite: true,
                output_folder: Some(folder.to_string_lossy().into_owned()),
                ..Default::default()
            }),
        )
        .await
        .context("adding the clip to the seeding session")?
        .into_handle()
        .context("the seeding session did not keep the clip")?;
    handle
        .wait_until_completed()
        .await
        .context("checking the clip")?;

    let peer = session
        .listen_addr()
        .context("the seeding session is not listening")?;
    Ok((peer, torrent.info_hash().as_string(), session))
}

/// Torrentio's answer for the clip, with the seeders and size in its title the
/// way Torrentio writes them ("👤 25 💾 1.23 MB ⚙️ source").
fn torrentio_streams(info_hash: &str, clip_len: u64) -> String {
    let mb = clip_len as f64 / (1024.0 * 1024.0);
    json!({
        "streams": [{
            "name": "Torrentio\n720p",
            "title": format!("{RELEASE_NAME}\n👤 {SEEDERS} 💾 {mb:.2} MB ⚙️ FakeTracker"),
            "infoHash": info_hash,
            "fileIdx": 0,
            "behaviorHints": {"bingeGroup": "torrentio|720p", "filename": RELEASE_NAME},
        }]
    })
    .to_string()
}

/// Every request target (path and query) the fake has answered, oldest first.
/// `GET /__requests` returns them as a JSON array, so a test can assert what
/// dekho actually sent.
static REQUESTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The body and status for one request path, query string already removed.
fn route<'a>(path: &str, streams: &'a str) -> (u16, &'a str) {
    let path = path.replace("%3A", ":").replace("%3a", ":");
    let movie = format!("/movie/{MOVIE_ID}");
    let show = format!("/tv/{SHOW_ID}");
    let season = format!("/tv/{SHOW_ID}/season/1");
    let movie_stream = format!("/stream/movie/{MOVIE_IMDB}.json");
    let show_stream = format!("/stream/series/{SHOW_IMDB}:1:");
    let person = "/person/900100";

    let body = match path.as_str() {
        p if p == movie => MOVIE,
        p if p == show => TV,
        p if p == season => SEASON_1,
        p if p == person => PERSON,
        "/search/multi" => SEARCH,
        p if p.starts_with("/trending/") => SEARCH,
        "/configuration/languages" => LANGUAGES,
        "/genre/movie/list" | "/genre/tv/list" => GENRES,
        p if p == movie_stream => streams,
        p if p.starts_with(&show_stream) && p.ends_with(".json") => streams,
        "/q.php" => APIBAY_EMPTY,
        // t5 routes
        "/movie/900003" => MOVIE_SE,
        "/configuration/countries" => COUNTRIES,
        "/person/900200" => PERSON_MARA,
        "/person/900200/tv_credits" => PERSON_MARA_TV,
        "/search/person" => SEARCH_PERSON,
        "/discover/movie" => DISCOVER_MOVIE,
        _ => return (404, NOT_FOUND),
    };
    (200, body)
}

/// Answer one GET and close the connection.
async fn serve(mut sock: TcpStream, streams: &str, hold_next: bool) -> Result<()> {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut buf).await?;
        anyhow::ensure!(n > 0 && head.len() < 64 * 1024, "bad request");
        head.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&head);
    let target = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .unwrap_or("/");
    let path = target.split('?').next().unwrap_or("/");

    if path == "/__requests" {
        let log = json!(&*REQUESTS.lock().unwrap()).to_string();
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{log}",
            log.len()
        );
        sock.write_all(reply.as_bytes()).await?;
        return Ok(sock.shutdown().await?);
    }
    REQUESTS.lock().unwrap().push(target.to_string());

    // t2 routes
    // `--hold-next` keeps Torrentio's answer for the second episode back for a
    // minute, so a test can act while dekho resolves the next episode.
    if hold_next && path.replace("%3A", ":").ends_with(&format!("{SHOW_IMDB}:1:2.json")) {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }

    let (status, body) = route(path, streams);
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let reply = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(reply.as_bytes()).await?;
    sock.shutdown().await?;
    Ok(())
}
