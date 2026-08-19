# dekho

Search a movie or series from the terminal and watch it in **mpv**. No browser,
no embed players, no tabs.

```sh
dekho fight club
dekho breaking bad
dekho severance -s 2 -e 5
```

Pick a title, pick an episode, and mpv opens. For a series the rest of the
season keeps playing on its own — the next episode is resolved and buffered
*while the current one plays*, so the changeover has no gap.

## How it works

```
TMDB (your key)  →  Torrentio  →  librqbit  →  http://127.0.0.1:PORT/…  →  mpv
   metadata         releases      streaming        range-capable
```

Nothing is downloaded before playback starts. librqbit prioritises pieces from
the playhead outward, so the swarm fetches ahead of where you are watching, and
mpv reads it over plain HTTP — which means seeking works normally.

This talks to **TMDB and Torrentio directly**. It does not use kojev.com's APIs,
so it costs nothing against that site's Workers budget.

## Why it doesn't buffer

The naive version of this tool picks the highest-quality release and stutters.
Quality alone cannot tell a streamable 4K WEB-DL (~20 Mbps) from a 4K Blu-ray
remux (~80 Mbps sustained, which no swarm reliably delivers). Four things
prevent that here:

1. **A bitrate ceiling.** Size ÷ runtime gives the bitrate a release *needs*.
   Anything over `--max-bitrate` (default 40 Mbps) is skipped before it wastes
   your time. This is what quietly excludes remuxes while keeping 4K WEB-DLs.
2. **A live throughput gate.** Before mpv is launched, dekho pulls from the head
   of the stream and measures what the swarm *actually delivers*. A release must
   sustain 1.25× its own bitrate to be played. Three details matter here, each
   of which was a real false positive during development:
   - The rate is a trailing 5-second window, never a peak. A swarm that bursts
     to 47 Mbps and decays to 0.6 must not pass.
   - It measures **sequential** bytes, not bytes acquired from the swarm. Pieces
     arrive out of order, so swarm progress can read 6.7 Mbps while the in-order
     stream mpv consumes trickles at 1.2.
   - Measurement does not start until the read position passes data that was
     already cached on disk. Draining a cached head at 3 Gbps says nothing about
     what happens when it runs out.
3. **Automatic downgrade.** A release that fails the gate is abandoned and the
   next-best one is tried, up to four. High quality is attempted first; smooth
   playback wins the tie.
4. **Two layers of buffer.** ~45 seconds is pre-buffered to disk before mpv
   starts, and mpv then keeps its own 300-second cache on top
   (`--cache-pause-initial`, `--demuxer-readahead-secs`), sized from the stream's
   bitrate. A brief swarm stall is absorbed silently instead of pausing playback.

If nothing clears the gate, the fastest release measured is played anyway — with
a warning saying so, rather than silently handing you something that stutters.

## Setup

Needs `mpv` on `PATH` and a free TMDB API key.

```sh
set -Ux TMDB_API_KEY <your key>   # fish
cargo build --release
install -m755 target/release/dekho ~/.local/bin/
```

Pieces are cached in `$XDG_CACHE_HOME/dekho` (override with `--download-dir`).
Re-watching something already there starts instantly. The cache is never pruned
automatically — 4K keeps a lot of disk, so clear it yourself when it grows.

## Options

| Flag | Default | Notes |
|---|---|---|
| `-q, --quality` | `4k` | Ceiling, not a target. `720p`, `1080p`, `4k`. |
| `--max-bitrate` | `40` | Mbps. Lower it on a weak connection; raise it for remuxes. |
| `--min-seeders` | `4` | Drops swarms that would only fail the probe. |
| `-s, --season` / `-e, --episode` | — | Skip the pickers. |
| `--no-next` | off | Play one episode instead of the rest of the season. |
| `-1, --first` | off | Take the top match instead of asking. With `-s`/`-e`, fully non-interactive. |
| `--dry-run` | off | Resolve and buffer, print what would play, then stop. Good for seeing which release the gate settles on. |

## Tests

```sh
cargo test                                              # unit tests, no network
cargo test --test stream_smoke -- --ignored --nocapture # live swarm, end to end
```

The smoke test streams Sintel (Creative Commons) through the whole chain:
session, metadata resolution, file selection and the local HTTP server.

## Note

dekho streams over BitTorrent, which means you upload while you watch, and what
is available on Torrentio is what is available. It plays what you point it at;
whether you have the right to is your call.
