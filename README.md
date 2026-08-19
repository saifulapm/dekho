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
| `-1, --first` | off | Take the top match without asking |
| `--dry-run` | off | Show what would play, then stop |
| `--download-dir` | `$XDG_CACHE_HOME/dekho` | Piece cache; never pruned automatically |

`dekho --help` and `dekho browse --help` list the rest.

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
