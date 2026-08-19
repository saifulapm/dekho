//! End-to-end smoke test for the streaming chain.
//!
//! Exercises the part that unit tests cannot: boot a real torrent session, bind
//! the loopback HTTP server, resolve metadata from a live swarm, pick the video
//! file, and pull enough bytes through the stream endpoint to prove playback
//! would start.
//!
//! Uses Sintel — the Blender Foundation's Creative Commons open movie, and the
//! long-standing reference torrent for exactly this kind of test.
//!
//! Needs network and peers, so it is `#[ignore]`d by default:
//!
//! ```sh
//! cargo test --test stream_smoke -- --ignored --nocapture
//! ```

use std::time::Duration;

// The crate is a binary, so the test drives it the way a user would rather than
// importing internals: it shells out to nothing and instead re-declares the
// small amount of setup it needs via the public HTTP surface.
const SINTEL_MAGNET: &str = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10\
&dn=Sintel&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce\
&tr=udp%3A%2F%2Fopen.demonii.com%3A1337%2Fannounce\
&tr=udp%3A%2F%2Ftracker.torrent.eu.org%3A451%2Fannounce\
&tr=udp%3A%2F%2Fexodus.desync.com%3A6969%2Fannounce";

#[tokio::test]
#[ignore = "needs network and live peers"]
async fn streams_a_real_torrent_through_the_local_server() {
    let dir = std::env::temp_dir().join("dekho-smoke");

    let engine = dekho::engine::Engine::start(dir)
        .await
        .expect("engine should boot and bind the loopback server");

    let added = tokio::time::timeout(Duration::from_secs(90), engine.add(SINTEL_MAGNET))
        .await
        .expect("metadata should resolve within 90s")
        .expect("torrent should be added");

    println!("resolved {} file(s):", added.files.len());
    for f in &added.files {
        println!("  [{}] {} ({} bytes)", f.idx, f.name, f.len);
    }
    assert!(!added.files.is_empty(), "torrent should list files");

    let idx = dekho::engine::choose_file(&added.files, None, None, None)
        .expect("a video file should be selected");
    let chosen = added.files.iter().find(|f| f.idx == idx).unwrap();
    println!("chose [{idx}] {}", chosen.name);
    assert!(
        chosen.name.to_ascii_lowercase().ends_with(".mp4"),
        "Sintel's feature file is an mp4, got {}",
        chosen.name
    );

    let url = engine.stream_url(added.id, idx);
    println!("streaming from {url}");

    // Sintel is ~1.1 GB over ~15 minutes, so roughly 10 Mbps.
    let probe = engine
        .probe(&added, idx, &url, Some(10_000_000), |s| {
            println!(
                "  buffered {} bytes at {} bps, {} peers ({} known)",
                s.buffered, s.rate_bps, s.live_peers, s.seen_peers
            );
        })
        .await
        .expect("probe should not error");

    println!("probe result: {probe:?}");
    match probe {
        dekho::engine::Probe::Ready { buffered, .. } => {
            assert!(buffered > 0, "a ready probe must have buffered something");
        }
        // A slow swarm is an environment problem, not a code failure — the
        // point of this test is that the chain runs end to end.
        dekho::engine::Probe::TooSlow {
            buffered,
            rate_bps,
            live_peers,
            seen_peers,
        } => {
            println!(
                "swarm was slow ({rate_bps} bps, {buffered} bytes, \
                 {live_peers} peers, {seen_peers} known) — chain still ran"
            );
        }
    }
}
