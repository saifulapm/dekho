# dekho

Search movies and TV series from the terminal and watch them in **mpv**. No
browser, no embed players, no tabs.

```sh
dekho fight club
dekho breaking bad -s 2 -e 1
dekho browse movies --sort top-rated --genre horror
```

Metadata comes from TMDB. Releases are streamed over BitTorrent — playback
starts in seconds, nothing is downloaded first, and seeking works.

## Install

Needs [mpv](https://mpv.io) on `PATH`, Rust, and a free
[TMDB API key](https://www.themoviedb.org/settings/api).

```sh
cargo build --release
install -m755 target/release/dekho ~/.local/bin/

set -Ux TMDB_API_KEY <your key>     # fish
export TMDB_API_KEY=<your key>      # bash/zsh
```

The environment always wins, but a desktop launcher has no exported shell, so
the key can live in `$XDG_CONFIG_HOME/dekho/config.toml` instead:

```toml
tmdb_api_key = "<your key>"
```

## Usage

**Search and play.** Pick a title, pick an episode, mpv opens. For a series the
rest of the season keeps playing on its own: the next episode is resolved and
buffered *while the current one plays*, so the changeover has no gap.

**Browse** when you don't know what you want yet. `⚙ Filters & sort` at the top
of every page changes sort, genre, language and minimum rating without leaving;
`--list` prints a page and exits instead, for piping.

```sh
dekho browse tv --lang korean --min-rating 8
dekho browse movies --lang bangla --sort top-rated --list
```

**Dual audio.** `--dual` ranks Hindi+English releases first and falls back when
a title has none; `--dual-only` refuses anything else. Detected from Torrentio's
language flags (`🇬🇧 / 🇮🇳`) and from release names (`Dual.Audio`, `Hin-Eng`).

```sh
dekho --dual inception
```

## Why it doesn't buffer

Picking the highest-quality release and hoping is how these tools stutter.
Quality alone cannot separate a streamable 4K WEB-DL (~20 Mbps) from a 4K remux
(~80 Mbps sustained, which no swarm reliably delivers). So dekho:

- skips releases whose **bitrate** (size ÷ runtime) exceeds `--max-bitrate`;
- **measures the swarm** before launching mpv, and requires 1.25× the release's
  own bitrate — on sequential bytes over a trailing window, not a peak, and not
  counting data already cached on disk;
- **downgrades automatically** to the next release when that fails, descending
  quality tiers rather than spending every attempt on 4K;
- **buffers twice** — ~45 s to disk, then mpv's own 300 s cache on top.

If nothing clears the check, the fastest release measured plays anyway, with a
warning rather than a silent stutter.

## Options

| Flag | Default | |
|---|---|---|
| `-q, --quality` | `4k` | Ceiling, not a target: `720p`, `1080p`, `4k` |
| `--dual` / `--dual-only` | off | Prefer / require Hindi+English |
| `--max-bitrate` | `40` | Mbps ceiling |
| `--min-seeders` | `4` | Drop hopeless swarms |
| `-s` / `-e` | — | Season / episode |
| `--no-next` | off | One episode instead of the season |
| `--resume` | off | Start where you stopped, at the episode you stopped in |
| `-1, --first` | off | Take the top match without asking |
| `--dry-run` | off | Show what would play, then stop |
| `--download-dir` | `$XDG_CACHE_HOME/dekho` | Piece cache; never pruned automatically |

`dekho --help` and `dekho browse --help` list the rest.

## Panel / scripting

Two entry points exist for programs rather than people. Both go through the same
resolution path as the interactive one, so a panel plays what the terminal would.

**`dekho api <verb>`** prints exactly one JSON object on stdout and exits 0 —
or `{"error":"…"}` and exits 1. Failure is on stdout too, so a caller parses one
stream and never reads stderr. No engine starts and no torrent is touched.

```sh
dekho api trending --kind movie --window week
dekho api discover --kind tv --lang korean --min-rating 8 --page 2
dekho api search fight club
dekho api title --id 1396 --kind tv          # detail, seasons included
dekho api episodes --id 1396 --season 2
dekho api genres --kind movie                # and: api languages
dekho api history --limit 10
dekho api prefetch --size w342 /ggFH.jpg /tsRy.jpg
```

`discover` takes `browse`'s exact `--sort/--genre/--lang/--min-rating`
vocabulary — a panel and the terminal disagreeing about what `--genre sci` means
would be worse than either being wrong. `prefetch` downloads TMDB images into
`$XDG_CACHE_HOME/dekho/img/<size>/`, eight at a time, skipping what is already
there, and answers with a local path each: a shell UI cannot do twenty TLS
handshakes quickly, and this is called on every open, so a dead image is missing
from `files` rather than fatal.

**`dekho play --id N --kind movie|tv`** never prompts — same release picking,
same throughput gate, same next-episode queueing, all the global flags. Without
`--json` it is an ordinary terminal command. With it, stdout is NDJSON, flushed
as each line happens:

```jsonc
{"event":"status","text":"Looking up releases for Fight Club (1999)…"}
{"event":"releases","found":141,"dual":7,"torrentio":50,"apibay":100,"shared":9}
{"event":"trying","quality":"1080p","size":"1.8 GB","seeders":2081,…}
{"event":"buffer","buffered":10485760,"rate_bps":13421341,…}
{"event":"ready",…}  {"event":"playing",…}  {"event":"queued",…}
{"event":"exit","code":0}
```

`exit` is always the last line, on every path; a failing one is preceded by
`{"event":"error","text":…}`.

**History** lives in `$XDG_STATE_HOME/dekho/history.json`, one entry per title
rather than per episode, capped at 100. Finishing an episode moves the entry to
the next one, so "continue" names what you would actually watch next. `--resume`
starts mpv there — but never inside the last minute of a title, which is a
finished one.

## Tests

```sh
cargo test                                              # unit, no network
cargo test --test stream_smoke -- --ignored --nocapture # live swarm, end to end
```

The smoke test streams [Sintel](https://durian.blender.org/) (Creative Commons)
through the whole chain.

## Note

dekho is a BitTorrent client: you upload while you watch, and it plays whatever
you point it at. Whether you have the right to is your call.

## Licence

MIT
