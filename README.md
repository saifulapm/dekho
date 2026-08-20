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
of every page changes sort, genre, language, decade and minimum rating without
leaving; `--list` prints a page and exits instead, for piping.

```sh
dekho browse tv --lang korean --min-rating 8
dekho browse movies --lang bangla --sort top-rated --list
dekho browse movies --genre horror --year 1990-1999
dekho browse movies --cast 287            # everything with this TMDB person
```

**Trailers** play in mpv like anything else, straight from YouTube — no
torrent, no engine, and nothing written to watch history.

```sh
dekho trailer --id 550 --kind movie
```

**Dual audio.** `--dual` ranks Hindi+English releases first and falls back when
a title has none; `--dual-only` refuses anything else. Detected from Torrentio's
language flags (`🇬🇧 / 🇮🇳`) and from release names (`Dual.Audio`, `Hin-Eng`).
When the preference is on, mpv opens on the Hindi track rather than the file's
default. Make it standing with `dual = "prefer"` (or `only`) in `config.toml`;
`--no-dual` turns it back off for one run.

```sh
dekho --dual inception
```

**Picking a release.** An interactive run asks which release to play the same
way it asks which title: best first, size, seeders and audio on every row, and
the list filters as you type — `hindi`, `dual`, `1080p`. The pick is committed:
a heavy release buffers for as long as it needs rather than being swapped for a
lighter one, and queued episodes stay inside the pack that was chosen. `-1` and
`dekho play` skip the menu and resolve automatically, which is the machinery
below.

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

## Why it doesn't buffer on a slow line either

Measuring one swarm at a time from scratch works when the link is fast and the
swarm is the question. On a slow link the link is the question, and the answer
is the same every night — so dekho remembers it. Every probe feeds a rolling
throughput estimate (`link.json` in the state dir; it rises fast and falls
slow, because a fast sample proves the link and a slow one may only prove the
swarm). Once three sessions agree:

- **The bitrate ceiling adapts.** Without an explicit `--max-bitrate`, releases
  are capped at 80% of the estimate, so a 5 Mbps line stops spending its first
  two attempts on 4K it could never stream. The flag always wins, and the
  adaptation is announced — one line saying what the link measured and what the
  cap is, an `adaptive` event in `--json`. You will never silently get 720p.
- **Swarm health outranks quality.** Below ~20 Mbps a well-seeded release a
  tier down beats a thinly-seeded one a tier up: any healthy swarm can fill a
  thin pipe, and an unhealthy one cannot, whatever its quality label says.
- **Thin headroom buys a bigger buffer.** A release that only just fits gets up
  to 240 s buffered before mpv starts instead of 45 s. One long wait up front
  beats pausing mid-scene — the same trade `--cache-pause-wait` makes inside
  mpv.
- **What's on disk plays instantly.** The release that played last time is
  remembered per title (`releases.json`) and tried first; if its opening slab
  is already cached, the probe skips the measurement and mpv starts now.
  Resuming and re-watching stop paying for the swarm at all.
- **The next episode waits its turn.** Queueing the next episode used to start
  the moment the current one did, which on a thin pipe steals bandwidth from
  the screen. By default it now waits until the current file is fully on disk
  or playback passes halfway (`--queue-next auto`); `immediate` restores the
  old behaviour, `50%` picks the moment yourself.

## Options

| Flag | Default | |
|---|---|---|
| `-q, --quality` | `4k` | Ceiling, not a target: `720p`, `1080p`, `4k` |
| `--dual` / `--dual-only` | off | Prefer / require Hindi+English |
| `--no-dual` | — | Ignore audio for this run, overriding a config default |
| `--max-bitrate` | adaptive | Mbps ceiling; derived from the remembered link when unset, never above 40 |
| `--min-seeders` | `4` | Drop hopeless swarms |
| `-s` / `-e` | — | Season / episode |
| `--no-next` | off | One episode instead of the season |
| `--queue-next` | `auto` | When the next episode starts buffering: `auto`, `immediate`, `50%` |
| `--resume` | off | Start where you stopped, at the episode you stopped in |
| `-1, --first` | off | Take the top match without asking |
| `--dry-run` | off | Show what would play, then stop |
| `--download-dir` | `$XDG_CACHE_HOME/dekho` | Piece cache |
| `--cache-max-gb` | `20` | Piece-cache budget in GiB; `0` disables pruning |

`dekho --help` and `dekho browse --help` list the rest. `cache_max_gb`,
`queue_next` and `dual` can live in `config.toml` beside the TMDB key; the
flags win.

## The caches

**Pieces.** The download dir is pruned to `--cache-max-gb` when a playback run
starts and again when it ends — never on a timer. Eviction is least-recently-
watched, except that titles history says are finished go first and anything
part-watched goes last, since those pieces are what makes resuming instant.
Whatever a live session is streaming is never touched, even from another dekho
process. `dekho cache status` shows the lot, `dekho cache clear` empties it.

**Metadata.** `dekho api` answers are cached on disk with a TTL per verb —
three days for a person, a day for a title's details and its videos, an hour for
trending and discover, fifteen minutes for a search. The people-shaped answers
last longest because they change slowest: a filmography gains a row every few
months. Anything served from disk carries `"cached": true` and `"age_secs"`.
When TMDB is unreachable an *expired* entry is served instead of an error,
marked `"stale": true`: a hub showing yesterday's trending beats a hub showing a
red line. `--refresh` forces the network; `history` and `prefetch` never touch
it.

## Panel / scripting

Three entry points exist for programs rather than people. All go through the
same resolution path as the interactive one, so a panel plays what the terminal
would.

**`dekho api <verb>`** prints exactly one JSON object on stdout and exits 0 —
or `{"error":"…"}` and exits 1. Failure is on stdout too, so a caller parses one
stream and never reads stderr. No engine starts and no torrent is touched.

```sh
dekho api trending --kind movie --window week
dekho api discover --kind tv --lang korean --min-rating 8 --page 2
dekho api discover --kind movie --cast 17419 --year 1990-1999
dekho api search fight club
dekho api title --id 1396 --kind tv          # detail: cast, crew, trailer, similar
dekho api videos --id 550 --kind movie       # trailers and clips, best first
dekho api person --id 17419                  # what a cast click resolves to
dekho api episodes --id 1396 --season 2
dekho api genres --kind movie                # and: api languages
dekho api history --limit 10
dekho api prefetch --size w185 /8nyt.jpg /ajNa.jpg
dekho api cache                              # the piece cache, for a panel to show
```

`discover` takes `browse`'s exact `--sort/--genre/--lang/--min-rating/--year/
--cast` vocabulary — a panel and the terminal disagreeing about what `--genre
sci` means would be worse than either being wrong. `prefetch` downloads TMDB
images into `$XDG_CACHE_HOME/dekho/img/<size>/`, eight at a time, skipping what
is already there, and answers with a local path each: a shell UI cannot do
twenty TLS handshakes quickly, and this is called on every open, so a dead image
is missing from `files` rather than fatal. `w45` and `h632` are there for faces,
which TMDB serves in sizes it does not serve posters in.

`title` gets its cast, crew, videos and similar titles in the *same* TMDB
request as the rest (`append_to_response`), because five round trips is five
round trips. Cast is capped at twenty and crew is filtered to the jobs a viewer
recognises — Director, Creator, Writer, Screenplay, Executive Producer, Composer
— then deduplicated, since TMDB files a writer-director under both.

**Browsing by person is two different queries.** For movies it is TMDB's own
`with_cast`. For series there is no such thing: `/discover/tv` *accepts*
`with_cast` and `with_people` and ignores them, answering with the unfiltered
catalog and an unchanged total, which looks like an answer and is not. So the
series case is built from the person's own TV credits and filtered here —
genre, language, rating, year and sort all still apply, paged twenty at a time
like everything else. The one rule not carried over is the vote-count floor: it
exists to keep obscure entries off page one of a quarter-million-title catalog,
and a career is not that.

`person` sorts a filmography by TMDB popularity, minus talk shows and the rows
where someone appears as themselves. Both are dropped on purpose: popularity is
dominated by things that air every weeknight, so without it Bryan Cranston's
page opens on *The Tonight Show* rather than Breaking Bad.

**`dekho play --id N --kind movie|tv`** never prompts — same release picking,
same throughput gate, same next-episode queueing, all the global flags. Without
`--json` it is an ordinary terminal command. With it, stdout is NDJSON, flushed
as each line happens:

```jsonc
{"event":"status","text":"Looking up releases for Fight Club (1999)…"}
{"event":"adaptive","source":"link","max_bitrate_bps":4400000,"link_bps":5500000,…}
{"event":"releases","found":141,"dual":7,"torrentio":50,"apibay":100,"shared":9}
{"event":"trying","quality":"1080p","size":"1.8 GB","seeders":2081,…}
{"event":"buffer","buffered":10485760,"rate_bps":13421341,…}
{"event":"ready","cached":false,…}  {"event":"playing",…}  {"event":"queued",…}
{"event":"exit","code":0}
```

`exit` is always the last line, on every path; a failing one is preceded by
`{"event":"error","text":…}`. `adaptive` says which bitrate ceiling applied and
why (`flag`, `link`, or `default`); `ready` with `"cached": true` means the
opening slab was already on disk and nothing was measured.

**`dekho trailer --id N --kind movie|tv`** speaks the same NDJSON with `--json`
— `status`, `playing`, `error`, `exit` — and reads the trailer off the cached
`title` answer, so pressing play on a detail view the panel has already shown
costs no round trip at all. A title with no trailer is an ordinary error and a
non-zero exit, not a crash. With `--json`, mpv's own terminal output is
discarded rather than left to interleave with the stream: mpv writes its status
line to *stdout*.

## The YouTube 403

mpv's ytdl hook picks a YouTube player client whose media URLs are refused from
here, so a trailer dies on `[ffmpeg] https: HTTP error 403 Forbidden` before a
frame is drawn — `c=ANDROID_VR` in the failing URL. Measured on this machine,
unauthenticated: `tv`, `ios` and `web` all 403; **`web_embedded` and `mweb`
play**. dekho therefore passes
`--ytdl-raw-options=extractor-args=youtube:player_client=web_embedded`.

YouTube changes this without notice. When trailers start failing again, that one
line in `player.rs` is what to re-test: try `mweb`, then cookies from a browser
profile. Nothing else in dekho is affected — torrents do not go near YouTube.

**State** lives in `$XDG_STATE_HOME/dekho/`: `history.json` (what you watched,
one entry per title rather than per episode, capped at 100 — finishing an
episode moves the entry to the next one, so "continue" names what you would
actually watch next), `link.json` (the throughput estimate), and
`releases.json` (which release played last time, per title). `--resume` starts
mpv where history says — but never inside the last minute of a title, which is
a finished one.

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
