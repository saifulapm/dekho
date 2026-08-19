//! dekho — search a movie or series and stream it straight into mpv.
//!
//! Nothing here talks to kojev.com. Metadata comes from TMDB with your own key,
//! releases come from Torrentio and apibay together (see `sources`), and
//! playback is a local torrent stream handed to mpv as an ordinary HTTP URL.
//!
//! The design goal that shapes everything is *no buffering*. See `pick` for how
//! a release is chosen, `engine` for the throughput gate that vetoes one the
//! swarm cannot sustain, and `player` for mpv's own cushion on top.
//!
//! Two of the entry points here are for programs rather than people: `api`
//! answers catalog and history questions as JSON, and `play` streams a title by
//! id without ever prompting. Both run through the same resolution path as the
//! interactive one, so what a panel plays is what the terminal would have.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use librqbit::ManagedTorrent;
use serde_json::{json, Value};

use dekho::browse::{self, Kind, Sort};
use dekho::engine::{self, Engine, Probe};
use dekho::history::{self, History, Recorder, Watch};
use dekho::link::{self, LinkStore};
use dekho::pick::{self, DualPreference, Filters, MAX_ATTEMPTS};
use dekho::player::{Event, Mode, Mpv, ProgressSink, QueueWhen};
use dekho::replay::{self, ReplayStore};
use dekho::sources::{self, Sources};
use dekho::tmdb::{Episode, MediaType, SearchHit, Show, Tmdb};
use dekho::torrentio::{self, format_bps, format_bytes, Quality};
use dekho::{api, apicache, cache, config, xdg};

#[derive(Parser)]
#[command(
    name = "dekho",
    version,
    about = "Search movies and series, stream them straight into mpv",
    // So `dekho fight club` still reaches `query` while `dekho browse` reaches
    // the subcommand. The cost is that a title literally named "browse" cannot
    // be searched for positionally.
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// What to search for, e.g. `dekho breaking bad`
    query: Vec<String>,

    /// Highest quality to consider: 720p, 1080p, 4k
    #[arg(short = 'q', long, default_value = "4k", global = true)]
    quality: String,

    /// Skip releases needing more than this many Mbps sustained. This is what
    /// separates a streamable 4K WEB-DL from an unstreamable 4K remux. When
    /// not given, dekho derives a ceiling from what this link has actually
    /// delivered in past sessions (never above the old default of 40).
    #[arg(long, global = true)]
    max_bitrate: Option<u64>,

    /// Skip releases with fewer seeders than this
    #[arg(long, default_value_t = 4, global = true)]
    min_seeders: u32,

    /// Prefer Hindi+English dual-audio releases, falling back to others when a
    /// title has none
    #[arg(long, global = true)]
    dual: bool,

    /// Play only dual-audio releases, and fail rather than settle for one track
    #[arg(long, global = true, conflicts_with = "dual")]
    dual_only: bool,

    /// Season to start from (series only; skips the picker)
    #[arg(short = 's', long, global = true)]
    season: Option<u32>,

    /// Episode to start from (series only; skips the picker)
    #[arg(short = 'e', long, global = true)]
    episode: Option<u32>,

    /// Play just the chosen episode instead of queueing the rest of the season
    #[arg(long, global = true)]
    no_next: bool,

    /// Pick up where you stopped, and for a series at the episode you stopped
    /// in. An explicit -s/-e wins over what history remembers.
    #[arg(long, global = true)]
    resume: bool,

    /// Take the top search match instead of asking. Makes the whole run
    /// non-interactive when combined with -s/-e.
    #[arg(short = '1', long, global = true)]
    first: bool,

    /// Resolve and buffer as usual, then print what would play and stop.
    /// Useful for checking which release the gate settles on.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Where to keep downloaded pieces (default: $XDG_CACHE_HOME/dekho)
    #[arg(long, global = true)]
    download_dir: Option<PathBuf>,

    /// Piece-cache budget in GiB; least-recently-watched titles are evicted
    /// past it, finished ones first. 0 disables pruning. Also settable as
    /// `cache_max_gb` in config.toml. Default: 20.
    #[arg(long, global = true)]
    cache_max_gb: Option<u64>,

    /// When a series queues the next episode: `auto` (once the current one is
    /// comfortably buffered — its default), `immediate` (the old behaviour),
    /// or a percentage of the way through, like `50%`. Also settable as
    /// `queue_next` in config.toml.
    #[arg(long, global = true)]
    queue_next: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Browse the catalog with filters and sorting, then play what you pick
    Browse(BrowseArgs),
    /// Play a title by TMDB id without asking anything
    Play(PlayArgs),
    /// Play a title's trailer in mpv
    Trailer(TrailerArgs),
    /// Answer catalog and history questions as JSON, for panels and scripts
    Api(ApiArgs),
    /// Show or clear the piece cache
    Cache(CacheArgs),
}

#[derive(Args)]
struct BrowseArgs {
    /// `movies` or `tv`. Omit to be asked.
    kind: Option<String>,

    /// popular, top-rated, newest, oldest, box-office
    #[arg(long)]
    sort: Option<String>,

    /// Genre name or id — `horror`, `sci`, `27`. Kind-specific.
    #[arg(long)]
    genre: Option<String>,

    /// Original language — `bn`, `bangla`, `ko`, `korean`
    #[arg(long)]
    lang: Option<String>,

    /// Minimum TMDB rating: 5-9
    #[arg(long)]
    min_rating: Option<u32>,

    /// A year like `1999`, or a range like `1990-1999`
    #[arg(long)]
    year: Option<String>,

    /// Only titles this TMDB person id is in — `dekho api person --id N` has
    /// the ids
    #[arg(long)]
    cast: Option<u32>,

    /// Print a page of results and exit instead of browsing interactively.
    /// Handy for piping, and for seeing what a filter combination yields.
    #[arg(long)]
    list: bool,

    /// Which page to start on (or to print with --list)
    #[arg(long, default_value_t = 1)]
    page: u32,
}

#[derive(Args)]
struct PlayArgs {
    /// TMDB id of the movie or show
    #[arg(long)]
    id: u32,

    /// `movie` or `tv`
    #[arg(long)]
    kind: String,

    /// Report progress as NDJSON on stdout, one object per line, instead of
    /// status lines on stderr
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TrailerArgs {
    /// TMDB id of the movie or show
    #[arg(long)]
    id: u32,

    /// `movie` or `tv`
    #[arg(long)]
    kind: String,

    /// Report progress as NDJSON on stdout, one object per line, instead of
    /// status lines on stderr
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ApiArgs {
    #[command(subcommand)]
    verb: ApiVerb,

    /// Skip the response cache and ask TMDB fresh. The answer is still
    /// written back, and a network failure still falls back to a stale one.
    #[arg(long, global = true)]
    refresh: bool,
}

#[derive(Args)]
struct CacheArgs {
    #[command(subcommand)]
    action: CacheAction,
}

#[derive(Subcommand)]
enum CacheAction {
    /// What the piece cache holds, and how it stands against the budget
    Status,
    /// Evict everything no live session is streaming from
    Clear,
}

#[derive(Subcommand)]
enum ApiVerb {
    /// Search movies and shows together
    Search {
        /// What to look for
        query: Vec<String>,
    },
    /// What is being watched right now
    Trending {
        /// movie, tv, or all
        #[arg(long, default_value = "all")]
        kind: String,
        /// day or week
        #[arg(long, default_value = "week")]
        window: String,
    },
    /// One page of the catalog, with `dekho browse`'s filters
    Discover {
        /// `movie` or `tv`
        #[arg(long)]
        kind: String,
        /// popular, top-rated, newest, oldest, box-office
        #[arg(long)]
        sort: Option<String>,
        /// Genre name or id — `horror`, `sci`, `27`. Kind-specific.
        #[arg(long)]
        genre: Option<String>,
        /// Original language — `bn`, `bangla`, `ko`, `korean`
        #[arg(long)]
        lang: Option<String>,
        /// Minimum TMDB rating: 5-9
        #[arg(long)]
        min_rating: Option<u32>,
        /// A year like `1999`, or a range like `1990-1999`
        #[arg(long)]
        year: Option<String>,
        /// Only titles this TMDB person id is in
        #[arg(long)]
        cast: Option<u32>,
        #[arg(long, default_value_t = 1)]
        page: u32,
    },
    /// Genre ids and names for one kind
    Genres {
        /// `movie` or `tv`
        #[arg(long)]
        kind: String,
    },
    /// Original languages the filters offer
    Languages,
    /// Everything about one title: cast, crew, trailer, similar titles
    Title {
        #[arg(long)]
        id: u32,
        /// `movie` or `tv`
        #[arg(long)]
        kind: String,
    },
    /// Trailers, teasers and clips for one title, best first
    Videos {
        #[arg(long)]
        id: u32,
        /// `movie` or `tv`
        #[arg(long)]
        kind: String,
    },
    /// One person and what they were in — what a cast click resolves to
    Person {
        #[arg(long)]
        id: u32,
    },
    /// Episodes of one season. Takes the season from the global --season.
    Episodes {
        #[arg(long)]
        id: u32,
    },
    /// Recently watched titles, newest first
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// The piece cache as JSON: budget, usage, and every entry's state
    Cache,
    /// Download TMDB images into the local cache and report their paths
    Prefetch {
        /// w45, w92, w154, w185, w342, w500, w780, h632 or original
        #[arg(long)]
        size: String,
        /// TMDB image paths, e.g. /pB8B.jpg
        paths: Vec<String>,
    },
}

/// One selectable line in the catalog browser.
enum Row {
    Title(Box<SearchHit>),
    NextPage(u32, u32),
    PrevPage(u32),
    Settings(String),
    Quit,
}

impl std::fmt::Display for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Row::Title(h) => {
                let name = if h.year.is_empty() {
                    h.title.clone()
                } else {
                    format!("{} ({})", h.title, h.year)
                };
                // Pad by char count, not bytes: titles are routinely non-ASCII
                // and byte padding would ragged the ★ column.
                let pad = 52usize.saturating_sub(name.chars().count());
                write!(f, "{name}{:pad$}  ★ {:.1}", "", h.vote)
            }
            Row::NextPage(next, total) => write!(f, "→  Next page ({next}/{total})"),
            Row::PrevPage(prev) => write!(f, "←  Previous page ({prev})"),
            Row::Settings(summary) => write!(f, "⚙  Filters & sort — {summary}"),
            Row::Quit => write!(f, "✕  Quit"),
        }
    }
}

/// An entry in the filters submenu.
enum Setting {
    Sort,
    Genre,
    Language,
    Rating,
    Year,
    SwitchKind(Kind),
    Reset,
    Back,
}

impl std::fmt::Display for Setting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Setting::Sort => write!(f, "Sort"),
            Setting::Genre => write!(f, "Genre"),
            Setting::Language => write!(f, "Language"),
            Setting::Rating => write!(f, "Minimum rating"),
            Setting::Year => write!(f, "Decade"),
            Setting::SwitchKind(k) => write!(f, "Switch to {k}"),
            Setting::Reset => write!(f, "Reset filters"),
            Setting::Back => write!(f, "← Back to the list"),
        }
    }
}

/// A labelled choice, so option lists can carry a value the label does not.
struct Choice<T> {
    label: String,
    value: T,
}

impl<T> std::fmt::Display for Choice<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// A release that cleared the gate and is ready for mpv.
struct Playable {
    url: String,
    title: String,
    bitrate: Option<u64>,
    /// Kept so losing candidates can be dropped from the session.
    torrent_id: usize,
    /// The live torrent and file being played, so the queue-next trigger can
    /// ask how much of it is on disk.
    handle: Arc<ManagedTorrent>,
    file_idx: usize,
    file_len: u64,
    /// The top-level cache entry the torrent writes into, for LRU touching.
    entry_name: Option<String>,
}

/// What the deferred-queue loop needs to know about the episode on screen.
struct PlayInfo {
    handle: Arc<ManagedTorrent>,
    file_idx: usize,
    file_len: u64,
}

impl PlayInfo {
    fn of(p: &Playable) -> Self {
        Self {
            handle: p.handle.clone(),
            file_idx: p.file_idx,
            file_len: p.file_len,
        }
    }

    /// Whether the file on screen is fully on disk — the moment queueing the
    /// next episode stops costing the stream anything.
    fn downloaded(&self) -> bool {
        self.file_len > 0 && Engine::have_bytes(&self.handle, self.file_idx) >= self.file_len
    }
}

/// Where a playback run reports to.
///
/// The two modes carry the same information: status lines on stderr for a
/// person, one JSON object per line on stdout for whatever is rendering it.
/// Nothing in the resolution path knows which it is talking to, so `play`
/// stays pleasant in a terminal without the panel being a special case.
struct Out {
    json: bool,
}

impl Out {
    fn emit(&self, value: Value) {
        let mut out = std::io::stdout();
        // NDJSON is only useful live, and a caller blocked on the next line
        // cannot wait for a buffer to fill.
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    }

    /// A durable line with nothing more structured to say about it.
    fn status(&self, text: &str) {
        if self.json {
            self.emit(json!({"event": "status", "text": text}));
        } else {
            status(text);
        }
    }

    /// A durable line that is also a typed event.
    fn event(&self, text: &str, make: impl FnOnce() -> Value) {
        if self.json {
            self.emit(make());
        } else {
            status(text);
        }
    }

    /// A reading that is overwritten in place for a person, and is one event
    /// like any other for a caller.
    fn tick(&self, text: &str, make: impl FnOnce() -> Value) {
        if self.json {
            self.emit(make());
        } else {
            progress(text);
        }
    }

    fn clear(&self) {
        if !self.json {
            clear_progress();
        }
    }

    /// Close the stream. `exit` is always the last line, on every path,
    /// including the ones that failed.
    fn finish(&self, result: Result<()>) -> Result<()> {
        if !self.json {
            return result;
        }
        match &result {
            Ok(()) => self.emit(json!({"event": "exit", "code": 0})),
            Err(e) => {
                self.emit(json!({"event": "error", "text": format!("{e:#}")}));
                self.emit(json!({"event": "exit", "code": 1}));
            }
        }
        result
    }
}

/// Everything a playback run needs that does not change between episodes.
///
/// The interactive path and `dekho play` differ only in what they set here.
struct Run<'a> {
    tmdb: &'a Tmdb,
    sources: &'a Sources,
    engine: &'a Engine,
    filters: &'a Filters,
    out: &'a Out,
    history: Arc<Recorder>,
    /// The remembered link throughput, fed by every probe that measured one.
    link: &'a LinkStore,
    /// Which release played last time per title, so resume tries it first.
    replay: &'a ReplayStore,
    /// Where the bitrate ceiling came from, for honest error messages.
    ceiling: link::Ceiling,
    /// When a series queues the next episode.
    queue_when: QueueWhen,
    download_dir: &'a Path,
    season: Option<u32>,
    episode: Option<u32>,
    no_next: bool,
    resume: bool,
    dry_run: bool,
    /// Whether a missing season or episode may be asked for. `play` never asks.
    interactive: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Answered before anything boots: an `api` verb starts no engine, adds no
    // torrent, and for history and prefetch does not even need a TMDB key.
    if let Some(Command::Api(args)) = &cli.command {
        return run_api(args, &cli).await;
    }
    // Same for the cache verbs: they read the disk, not the network.
    if let Some(Command::Cache(args)) = &cli.command {
        return run_cache(args, &cli);
    }
    // And for a trailer: it is an ordinary HTTP stream from YouTube, so none
    // of the torrent machinery below applies to it.
    if let Some(Command::Trailer(args)) = &cli.command {
        return run_trailer(args).await;
    }

    let max_quality = Quality::parse_cap(&cli.quality)
        .with_context(|| format!("unknown --quality {:?}; try 720p, 1080p or 4k", cli.quality))?;

    // The bitrate ceiling: an explicit flag wins, otherwise what the link has
    // shown it can carry, otherwise the historical default.
    let link_store = LinkStore::open(link::path());
    let snapshot = link_store.snapshot();
    let ceiling = link::effective_ceiling(
        cli.max_bitrate.map(|m| m.saturating_mul(1_000_000)),
        &snapshot,
    );

    let filters = Filters {
        max_quality,
        max_bitrate: ceiling.bps(),
        min_seeders: cli.min_seeders,
        dual: match (cli.dual, cli.dual_only) {
            (_, true) => DualPreference::Only,
            (true, _) => DualPreference::Prefer,
            _ => DualPreference::Ignore,
        },
        link_bps: snapshot.trusted_estimate(),
    };

    let out = Out {
        json: matches!(&cli.command, Some(Command::Play(p)) if p.json),
    };
    let result = run(&cli, &filters, ceiling, &link_store, &out).await;
    out.finish(result)
}

async fn run(
    cli: &Cli,
    filters: &Filters,
    ceiling: link::Ceiling,
    link_store: &LinkStore,
    out: &Out,
) -> Result<()> {
    let tmdb = Tmdb::new(config::tmdb_key()?)?;
    let sources = Sources::new()?;

    // Resolve what to watch before booting the engine, so a typo or an empty
    // catalog costs nothing.
    let mut target: Option<(u32, MediaType)> = None;
    let mut browse_state: Option<(browse::Filters, u32)> = None;

    match &cli.command {
        Some(Command::Api(_)) | Some(Command::Cache(_)) | Some(Command::Trailer(_)) => {
            unreachable!("answered before the engine boots")
        }
        Some(Command::Play(args)) => {
            let kind = browse::parse_kind(&args.kind)
                .with_context(|| format!("unknown --kind {:?}; use `movie` or `tv`", args.kind))?;
            target = Some((args.id, MediaType::of(kind)));
        }
        Some(Command::Browse(args)) => {
            let bf = initial_filters(args)?;
            if args.list {
                return print_page(&tmdb, &bf, args.page).await;
            }
            browse_state = Some((bf, args.page.max(1)));
        }
        None => {
            let query = cli.query.join(" ");
            anyhow::ensure!(
                !query.trim().is_empty(),
                "nothing to search for — try `dekho fight club` or `dekho browse`"
            );
            out.status(&format!("Searching TMDB for {query:?}…"));
            let mut hits = tmdb.search(&query).await?;
            anyhow::ensure!(!hits.is_empty(), "nothing on TMDB matched {query:?}");
            let hit = if cli.first {
                let top = hits.remove(0);
                out.status(&format!("→  {top}"));
                top
            } else {
                choose("What do you want to watch?", hits)?
            };
            target = Some((hit.id, hit.media_type));
        }
    }

    // --- boot the engine ----------------------------------------------------
    let download_dir = cli
        .download_dir
        .clone()
        .unwrap_or_else(default_download_dir);
    out.status(&format!("Cache: {}", download_dir.display()));

    // Bring the piece cache under budget before the engine starts filling it.
    // LRU plus the finished-first rule keeps what a resume would want; other
    // processes' live torrents are protected by their lock files.
    let budget = cache::budget_bytes(cli.cache_max_gb);
    report_prune(
        out,
        &cache::prune(
            &download_dir,
            budget,
            &History::load(&history::path()),
            &Default::default(),
        ),
    );

    let engine = Engine::start(download_dir.clone()).await?;

    // Say which bitrate ceiling applies and why. Humans only hear about it
    // when the link actually adapted; a JSON caller always gets the event, so
    // a panel can explain a 720p pick without guessing.
    if let link::Ceiling::Adapted {
        bps,
        link_bps,
        samples,
    } = ceiling
    {
        out.event(
            &format!(
                "Link remembered at {} over {samples} session{} — capping releases at {} \
                 (override with --max-bitrate)",
                format_bps(link_bps),
                if samples == 1 { "" } else { "s" },
                format_bps(bps),
            ),
            || ceiling.json(),
        );
    } else if out.json {
        out.emit(ceiling.json());
    }

    let queue_when = match cli
        .queue_next
        .clone()
        .or_else(config::queue_next)
    {
        Some(v) => QueueWhen::parse(&v).with_context(|| {
            format!("unknown --queue-next {v:?}; use `auto`, `immediate`, or a percentage like 50%")
        })?,
        None => QueueWhen::Auto,
    };

    let replay_store = ReplayStore::open(replay::path());
    let run = Run {
        tmdb: &tmdb,
        sources: &sources,
        engine: &engine,
        filters,
        out,
        history: Recorder::new(history::path()),
        link: link_store,
        replay: &replay_store,
        ceiling,
        queue_when,
        download_dir: &download_dir,
        season: cli.season,
        episode: cli.episode,
        no_next: cli.no_next,
        resume: cli.resume,
        dry_run: cli.dry_run,
        interactive: !matches!(cli.command, Some(Command::Play(_))),
    };

    let result = match (target, browse_state) {
        (Some((id, kind)), _) => run.play(id, kind).await,
        (None, Some((bf, page))) => browse_loop(&run, bf, page).await,
        (None, None) => unreachable!("one of the two branches above always applies"),
    };

    // Prune again now that playback wrote new pieces; whatever this session
    // still holds is protected on top of other processes' locks.
    report_prune(
        out,
        &cache::prune(
            &download_dir,
            budget,
            &History::load(&history::path()),
            &engine.active_names(),
        ),
    );

    result
}

/// One status line when pruning actually removed something, silence otherwise.
fn report_prune(out: &Out, report: &cache::PruneReport) {
    if report.evicted.is_empty() {
        return;
    }
    out.status(&format!(
        "Cache: evicted {} title{} ({}) to stay under budget",
        report.evicted.len(),
        if report.evicted.len() == 1 { "" } else { "s" },
        format_bytes(report.freed),
    ));
}

/// Run one `api` verb and print its object. Failure is an object too — a caller
/// must never have to read stderr to find out what went wrong.
///
/// The TMDB-backed verbs go through the disk cache: a fresh entry answers
/// without the network, and when the network fails a stale entry answers
/// instead of an error, marked so the caller can tell. `--refresh` skips the
/// fresh read but keeps the stale fallback — a forced refresh that cannot
/// reach TMDB is still better answered with yesterday's data than a red line.
async fn run_api(args: &ApiArgs, cli: &Cli) -> Result<()> {
    // Write errors are dropped rather than reported: piping into `head` closes
    // the pipe, and a broken-pipe complaint on stderr would be the one thing on
    // stderr this command ever prints.
    let mut out = std::io::stdout();
    let plan = cache_plan(&args.verb, cli);

    match apicache::serve(plan.as_ref(), args.refresh, api_value(&args.verb, cli)).await {
        Ok(value) => {
            let _ = writeln!(out, "{value}");
            let _ = out.flush();
            Ok(())
        }
        Err(e) => {
            let _ = writeln!(out, "{}", api::error(&format!("{e:#}")));
            let _ = out.flush();
            std::process::exit(1);
        }
    }
}

/// The cache key and TTL for a verb, or `None` for the verbs that must stay
/// live: `history` and `prefetch` are local, `genres`/`languages` are compiled
/// in, and `cache` reports the disk it stands on.
fn cache_plan(verb: &ApiVerb, cli: &Cli) -> Option<(String, u64)> {
    fn norm(s: &str) -> String {
        s.trim().to_ascii_lowercase()
    }
    let parts: Vec<String> = match verb {
        ApiVerb::Search { query } => vec!["search".into(), norm(&query.join(" "))],
        ApiVerb::Trending { kind, window } => {
            vec!["trending".into(), norm(kind), norm(window)]
        }
        ApiVerb::Discover {
            kind,
            sort,
            genre,
            lang,
            min_rating,
            year,
            cast,
            page,
        } => vec![
            "discover".into(),
            norm(kind),
            norm(sort.as_deref().unwrap_or_default()),
            norm(genre.as_deref().unwrap_or_default()),
            norm(lang.as_deref().unwrap_or_default()),
            min_rating.map(|r| r.to_string()).unwrap_or_default(),
            // Part of the key or the un-filtered page would answer for the
            // filtered one — the whole point of a cast click is a different
            // list.
            norm(year.as_deref().unwrap_or_default()),
            cast.map(|c| c.to_string()).unwrap_or_default(),
            page.to_string(),
        ],
        ApiVerb::Title { id, kind } => vec!["title".into(), id.to_string(), norm(kind)],
        ApiVerb::Videos { id, kind } => vec!["videos".into(), id.to_string(), norm(kind)],
        ApiVerb::Person { id } => vec!["person".into(), id.to_string()],
        ApiVerb::Episodes { id } => vec![
            "episodes".into(),
            id.to_string(),
            cli.season.map(|s| s.to_string()).unwrap_or_default(),
        ],
        _ => return None,
    };
    let ttl = apicache::ttl_secs(&parts[0])?;
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    Some((apicache::key(&refs), ttl))
}

/// `dekho cache status` and `dekho cache clear`, for people. The JSON shape
/// lives at `dekho api cache`.
fn run_cache(args: &CacheArgs, cli: &Cli) -> Result<()> {
    let dir = cli
        .download_dir
        .clone()
        .unwrap_or_else(default_download_dir);
    let budget = cache::budget_bytes(cli.cache_max_gb);
    match args.action {
        CacheAction::Status => {
            let history = History::load(&history::path());
            let value = cache::status_value(&dir, budget, &history);
            let used = value["used_bytes"].as_u64().unwrap_or(0);
            println!("cache:  {}", dir.display());
            match budget {
                0 => println!("used:   {} (no budget — pruning disabled)", format_bytes(used)),
                _ => println!(
                    "used:   {} of {} budget",
                    format_bytes(used),
                    format_bytes(budget)
                ),
            }
            let entries = value["entries"].as_array().cloned().unwrap_or_default();
            if entries.is_empty() {
                println!("(no titles cached)");
                return Ok(());
            }
            for e in entries {
                println!(
                    "{:>9}  {:<12}  {}{}",
                    format_bytes(e["bytes"].as_u64().unwrap_or(0)),
                    e["state"].as_str().unwrap_or("unknown"),
                    e["name"].as_str().unwrap_or("?"),
                    if e["active"].as_bool().unwrap_or(false) {
                        "  (active)"
                    } else {
                        ""
                    },
                );
            }
        }
        CacheAction::Clear => {
            let report = cache::clear(&dir);
            println!(
                "evicted {} title{}, freed {}",
                report.evicted.len(),
                if report.evicted.len() == 1 { "" } else { "s" },
                format_bytes(report.freed),
            );
            if report.used_after > 0 {
                println!(
                    "{} kept: held by a live session",
                    format_bytes(report.used_after)
                );
            }
        }
    }
    Ok(())
}

async fn api_value(verb: &ApiVerb, cli: &Cli) -> Result<Value> {
    // Built where it is needed, so `history` and `prefetch` work without a key.
    let tmdb = || -> Result<Tmdb> { Tmdb::new(config::tmdb_key()?) };

    match verb {
        ApiVerb::Search { query } => {
            let query = query.join(" ");
            anyhow::ensure!(!query.trim().is_empty(), "nothing to search for");
            api::search(&tmdb()?, &query).await
        }
        ApiVerb::Trending { kind, window } => {
            api::trending(&tmdb()?, trending_kind(kind)?, trending_window(window)?).await
        }
        ApiVerb::Discover {
            kind,
            sort,
            genre,
            lang,
            min_rating,
            year,
            cast,
            page,
        } => {
            let filters = browse::filters_from(
                api_kind(kind)?,
                sort.as_deref(),
                genre.as_deref(),
                lang.as_deref(),
                *min_rating,
                *cast,
                year.as_deref(),
            )?;
            api::discover(&tmdb()?, &filters, *page).await
        }
        ApiVerb::Genres { kind } => Ok(api::genres(api_kind(kind)?)),
        ApiVerb::Languages => Ok(api::languages()),
        ApiVerb::Title { id, kind } => {
            api::title(&tmdb()?, *id, MediaType::of(api_kind(kind)?)).await
        }
        ApiVerb::Videos { id, kind } => {
            api::videos(&tmdb()?, *id, MediaType::of(api_kind(kind)?)).await
        }
        ApiVerb::Person { id } => api::person(&tmdb()?, *id).await,
        ApiVerb::Episodes { id } => {
            let season = cli.season.context("api episodes needs --season")?;
            api::episodes(&tmdb()?, *id, season).await
        }
        ApiVerb::History { limit } => api::history(*limit),
        ApiVerb::Prefetch { size, paths } => api::prefetch(size, paths).await,
        ApiVerb::Cache => {
            let dir = cli
                .download_dir
                .clone()
                .unwrap_or_else(default_download_dir);
            let budget = cache::budget_bytes(cli.cache_max_gb);
            Ok(cache::status_value(
                &dir,
                budget,
                &History::load(&history::path()),
            ))
        }
    }
}

fn api_kind(value: &str) -> Result<Kind> {
    browse::parse_kind(value)
        .with_context(|| format!("unknown --kind {value:?}; use `movie` or `tv`"))
}

/// `--kind` for trending, where "all" is also an answer.
fn trending_kind(value: &str) -> Result<Option<MediaType>> {
    if value.trim().eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    Ok(Some(MediaType::of(
        browse::parse_kind(value)
            .with_context(|| format!("unknown --kind {value:?}; use `movie`, `tv` or `all`"))?,
    )))
}

fn trending_window(value: &str) -> Result<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "day" => Ok("day"),
        "week" => Ok("week"),
        other => anyhow::bail!("unknown --window {other:?}; use `day` or `week`"),
    }
}

/// Print one page of the catalog to stdout and stop. No engine, no torrents.
async fn print_page(tmdb: &Tmdb, bf: &browse::Filters, page: u32) -> Result<()> {
    let catalog = tmdb.discover(bf, page.max(1)).await?;
    eprintln!(
        "{} · {} · page {}/{}",
        bf.kind,
        bf.summary(),
        catalog.page,
        catalog.total_pages
    );
    if catalog.items.is_empty() {
        eprintln!("(nothing matched)");
        return Ok(());
    }
    for h in &catalog.items {
        let year = if h.year.is_empty() {
            String::new()
        } else {
            format!(" ({})", h.year)
        };
        println!("{:>4.1}  {}{}", h.vote, h.title, year);
    }
    Ok(())
}

/// Turn `browse` flags into a starting filter set, asking for the kind when it
/// was not given.
fn initial_filters(args: &BrowseArgs) -> Result<browse::Filters> {
    let kind = match args.kind.as_deref().map(str::trim) {
        None => choose("What are you in the mood for?", vec![Kind::Movie, Kind::Tv])?,
        Some(k) => browse::parse_kind(k)
            .with_context(|| format!("unknown kind {k:?}; use `movies` or `tv`"))?,
    };
    browse::filters_from(
        kind,
        args.sort.as_deref(),
        args.genre.as_deref(),
        args.lang.as_deref(),
        args.min_rating,
        args.cast,
        args.year.as_deref(),
    )
}

/// `dekho trailer` — the film's trailer in mpv, and nothing else.
///
/// Deliberately not a playback run: no engine boots, no torrent is touched,
/// and watch history is left alone, because a trailer is not something anyone
/// resumes.
async fn run_trailer(args: &TrailerArgs) -> Result<()> {
    let out = Out { json: args.json };
    let result = trailer(args, &out).await;
    out.finish(result)
}

async fn trailer(args: &TrailerArgs, out: &Out) -> Result<()> {
    let kind = MediaType::of(api_kind(&args.kind)?);
    let (id, key) = (args.id, kind.key());

    // The `title` verb already carries the best trailer, so this reuses its
    // cache entry rather than adding one of its own: a panel that has just
    // shown the detail view has warmed it, and pressing ▶ costs no round trip
    // at all. The key has to match `cache_plan`'s exactly for that to happen.
    out.status("Looking up the trailer…");
    let plan = apicache::ttl_secs("title")
        .map(|ttl| (apicache::key(&["title", &id.to_string(), key]), ttl));
    let value = apicache::serve(plan.as_ref(), false, async {
        api::title(&Tmdb::new(config::tmdb_key()?)?, id, kind).await
    })
    .await?;

    let name = value["title"].as_str().unwrap_or("this title");
    let year = value["year"].as_str().unwrap_or_default();
    let label = if year.is_empty() {
        format!("{name} — Trailer")
    } else {
        format!("{name} ({year}) — Trailer")
    };

    // No trailer is an ordinary answer, not a crash: plenty of older titles
    // have none, and TMDB simply has nothing to point mpv at.
    let url = value["trailer"].as_str().unwrap_or_default();
    anyhow::ensure!(!url.is_empty(), "TMDB has no trailer for {name}");

    out.event(&format!("▶  {label}"), || {
        json!({
            "event": "playing",
            "title": label,
            "kind": key,
            "id": id,
            "url": url,
        })
    });

    let mut mpv = Mpv::launch(url, &label, Mode::Trailer, None, None, out.json).await?;
    mpv.wait().await
}

/// The catalog browser: page through results, adjust filters, play a pick, and
/// come back to the same place afterwards.
async fn browse_loop(run: &Run<'_>, mut bf: browse::Filters, start_page: u32) -> Result<()> {
    let mut page: u32 = start_page.max(1);

    loop {
        run.out
            .status(&format!("Loading {} · {}…", bf.kind, bf.summary()));
        let catalog = run.tmdb.discover(&bf, page).await?;

        if catalog.items.is_empty() {
            run.out.status("Nothing matched those filters.");
            edit_filters(&mut bf)?;
            page = 1;
            continue;
        }

        let has_next = catalog.has_next();
        let total_pages = catalog.total_pages;

        // Filters go FIRST. A page holds twenty titles and only about fifteen
        // rows are ever on screen, so anything after them is invisible — put
        // this at the bottom and the browser looks like it cannot filter at all.
        let mut rows: Vec<Row> = vec![Row::Settings(bf.summary())];
        rows.extend(catalog.items.into_iter().map(|h| Row::Title(Box::new(h))));
        if has_next {
            rows.push(Row::NextPage(page + 1, total_pages));
        }
        if page > 1 {
            rows.push(Row::PrevPage(page - 1));
        }
        rows.push(Row::Quit);

        let header = format!(
            "{} · {} · page {}/{}",
            bf.kind,
            bf.summary(),
            page,
            total_pages
        );

        // Start on the first title, not on the filters row above it, so a quick
        // Enter plays something rather than opening a menu.
        match choose_at(
            &header,
            rows,
            1,
            Some("↑↓ move · enter select · type to search this page · ⚙ to change sort/genre/language"),
        )? {
            Row::Title(hit) => {
                run.play(hit.id, hit.media_type).await?;
                // Back to the same page, so one sitting can watch several things.
            }
            Row::NextPage(n, _) => page = n,
            Row::PrevPage(p) => page = p,
            Row::Settings(_) => {
                if edit_filters(&mut bf)? {
                    // Any filter change invalidates the current page number.
                    page = 1;
                }
            }
            Row::Quit => return Ok(()),
        }
    }
}

/// Show the filters submenu until the user goes back. Returns whether anything
/// changed.
///
/// Loops rather than applying one change and returning: setting a genre *and* a
/// sort is the normal case, and bouncing back to the list in between means
/// re-fetching a page nobody asked to see. The prompt carries the live summary,
/// so each change is visible as it is made.
fn edit_filters(bf: &mut browse::Filters) -> Result<bool> {
    let mut changed = false;
    loop {
        if edit_one(bf, &mut changed)? {
            return Ok(changed);
        }
    }
}

/// One pass of the filters menu. Returns true when the user is done.
fn edit_one(bf: &mut browse::Filters, changed: &mut bool) -> Result<bool> {
    let other = match bf.kind {
        Kind::Movie => Kind::Tv,
        Kind::Tv => Kind::Movie,
    };
    let menu = vec![
        Setting::Back,
        Setting::Sort,
        Setting::Genre,
        Setting::Language,
        Setting::Year,
        Setting::Rating,
        Setting::SwitchKind(other),
        Setting::Reset,
    ];

    match choose(&format!("{} — {}", bf.kind, bf.summary()), menu)? {
        Setting::Sort => {
            let opts: Vec<Choice<Sort>> = Sort::all(bf.kind)
                .into_iter()
                .map(|s| Choice {
                    label: s.label().to_string(),
                    value: s,
                })
                .collect();
            bf.sort = choose("Sort by", opts)?.value;
        }
        Setting::Genre => {
            let mut opts = vec![Choice {
                label: "All genres".into(),
                value: 0u32,
            }];
            opts.extend(browse::genres_for(bf.kind).iter().map(|(id, n)| Choice {
                label: (*n).to_string(),
                value: *id,
            }));
            bf.genre_id = choose("Genre", opts)?.value;
        }
        Setting::Language => {
            let mut opts = vec![Choice {
                label: "All languages".into(),
                value: String::new(),
            }];
            opts.extend(browse::LANGUAGES.iter().map(|(code, n)| Choice {
                label: (*n).to_string(),
                value: (*code).to_string(),
            }));
            bf.language = choose("Original language", opts)?.value;
        }
        Setting::Year => {
            // Decades, because that is how anyone actually asks: nobody
            // browses "1997". The current decade is bounded by the sort's own
            // "not in the future" rule, so it needs no special case here.
            let current = browse::this_year();
            let newest = current - current % 10;
            let mut opts = vec![Choice {
                label: "Any year".into(),
                value: None,
            }];
            opts.extend((0..12).map(|i| {
                let from = newest - i * 10;
                Choice {
                    label: format!("{from}s"),
                    value: Some((from, from + 9)),
                }
            }));
            bf.years = choose("Decade", opts)?.value;
        }
        Setting::Rating => {
            let opts: Vec<Choice<u32>> = std::iter::once(Choice {
                label: "Any rating".into(),
                value: 0u32,
            })
            .chain([9u32, 8, 7, 6, 5].into_iter().map(|r| Choice {
                label: format!("{r}+"),
                value: r,
            }))
            .collect();
            bf.min_rating = choose("Minimum rating", opts)?.value;
        }
        Setting::SwitchKind(k) => {
            // Genre ids do not carry across: 28 is Action for movies and
            // nothing at all for TV, so keeping it would silently empty the
            // list. Sort can carry, except box office, which TV has no data for.
            bf.kind = k;
            bf.genre_id = 0;
            if !Sort::all(k).contains(&bf.sort) {
                bf.sort = Sort::Popular;
            }
        }
        Setting::Reset => *bf = browse::Filters::new(bf.kind),
        // The only way out; everything else loops so several filters can be set
        // in one visit.
        Setting::Back => return Ok(true),
    }
    *changed = true;
    Ok(false)
}

impl Run<'_> {
    async fn play(&self, id: u32, kind: MediaType) -> Result<()> {
        match kind {
            MediaType::Movie => self.play_movie(id).await,
            MediaType::Tv => self.play_series(id).await,
        }
    }

    fn sink(&self) -> Option<Arc<dyn ProgressSink>> {
        Some(self.history.clone())
    }

    /// Report what would play and stop, for `--dry-run`.
    fn report_dry_run(&self, playable: &Playable, then: Option<String>) -> Result<()> {
        if self.out.json {
            self.out.emit(json!({
                "event": "dry-run",
                "title": playable.title,
                "url": playable.url,
                "bitrate": playable.bitrate,
            }));
            return Ok(());
        }
        println!("would play: {}", playable.title);
        println!("stream:     {}", playable.url);
        match playable.bitrate {
            Some(b) => println!("bitrate:    {}", format_bps(b)),
            None => println!("bitrate:    unknown"),
        }
        if let Some(then) = then {
            println!("then:       {then}");
        }
        Ok(())
    }

    async fn play_movie(&self, id: u32) -> Result<()> {
        let movie = self.tmdb.movie(id).await?;
        anyhow::ensure!(
            !movie.imdb_id.is_empty(),
            "TMDB has no IMDB id for this movie, so no torrents can be looked up"
        );
        let label = if movie.year.is_empty() {
            movie.title.clone()
        } else {
            format!("{} ({})", movie.title, movie.year)
        };

        let playable = self
            .resolve(
                &movie.imdb_id,
                MediaType::Movie,
                None,
                None,
                movie.runtime_secs,
                &label,
            )
            .await?;

        if self.dry_run {
            return self.report_dry_run(&playable, None);
        }

        let start = self
            .resume
            .then(|| self.history.entry(id, "movie"))
            .flatten()
            .and_then(|e| e.resume_position());

        self.out.event(&format!("▶  {label}"), || {
            json!({
                "event": "playing",
                "title": label,
                "kind": "movie",
                "id": id,
                "resumed_at": start.unwrap_or(0),
                "bitrate": playable.bitrate,
            })
        });

        self.history.set_current(Watch {
            id,
            kind: "movie",
            title: movie.title.clone(),
            year: movie.year.clone(),
            poster: movie.poster.clone(),
            backdrop: movie.backdrop.clone(),
            season: None,
            episode: None,
            episode_name: None,
            next: None,
        });

        let mut mpv = Mpv::launch(
            &playable.url,
            &playable.title,
            Mode::Torrent {
                bitrate_bps: playable.bitrate,
            },
            start,
            self.sink(),
            // mpv's status line goes to stdout, where a caller is parsing
            // NDJSON.
            self.out.json,
        )
        .await?;
        let waited = mpv.wait().await;
        self.history.flush();
        waited
    }

    async fn play_series(&self, id: u32) -> Result<()> {
        let show = self.tmdb.show(id).await?;
        anyhow::ensure!(
            !show.imdb_id.is_empty(),
            "TMDB has no IMDB id for this show, so no torrents can be looked up"
        );

        let remembered = self.resume.then(|| self.history.entry(id, "tv")).flatten();

        let season = match self.season.or(remembered.as_ref().and_then(|e| e.season)) {
            Some(n) => match show.seasons.iter().find(|s| s.number == n) {
                Some(s) => s.clone(),
                // An explicit -s naming a season that does not exist is worth
                // reporting; a history entry from before a season was renumbered
                // is not, so that just starts the show over.
                None if self.season.is_some() => {
                    anyhow::bail!("{} has no season {n}", show.name)
                }
                None => show.seasons[0].clone(),
            },
            None if show.seasons.len() == 1 || !self.interactive => show.seasons[0].clone(),
            None => choose("Which season?", show.seasons.clone())?,
        };

        let episodes = self.tmdb.episodes(&show, season.number).await?;
        anyhow::ensure!(
            !episodes.is_empty(),
            "TMDB lists no episodes for season {}",
            season.number
        );

        // Only honour a remembered episode inside the season it was remembered
        // for; -s alone means "this season, from the top".
        let remembered_here = remembered
            .as_ref()
            .filter(|e| e.season == Some(season.number));
        let start_at = match self.episode.or(remembered_here.and_then(|e| e.episode)) {
            Some(n) => match episodes.iter().position(|e| e.number == n) {
                Some(i) => i,
                None if self.episode.is_some() => {
                    anyhow::bail!("season {} has no episode {n}", season.number)
                }
                None => 0,
            },
            None if !self.interactive => 0,
            None => {
                let chosen = choose("Which episode?", episodes.clone())?;
                episodes
                    .iter()
                    .position(|e| e.number == chosen.number)
                    .unwrap_or(0)
            }
        };

        // Everything from the chosen episode onward, so playback keeps going.
        let mut upcoming: VecDeque<Episode> = episodes.iter().skip(start_at).cloned().collect();
        let first = upcoming.pop_front().context("no episode to play")?;

        let start = remembered_here
            .filter(|e| e.episode == Some(first.number))
            .and_then(|e| e.resume_position());

        let playable = self.resolve_episode(&show, &first).await?;

        if self.dry_run {
            return self.report_dry_run(
                &playable,
                Some(
                    upcoming
                        .front()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "(end of season)".into()),
                ),
            );
        }

        let label = format!("{} — {first}", show.name);
        self.out.event(&format!("▶  {label}"), || {
            json!({
                "event": "playing",
                "title": label,
                "kind": "tv",
                "id": id,
                "resumed_at": start.unwrap_or(0),
                "bitrate": playable.bitrate,
            })
        });

        self.history
            .set_current(watch_for(&show, &first, &episodes));

        let mut mpv = Mpv::launch(
            &playable.url,
            &playable.title,
            Mode::Torrent {
                bitrate_bps: playable.bitrate,
            },
            start,
            self.sink(),
            // mpv's status line goes to stdout, where a caller is parsing
            // NDJSON.
            self.out.json,
        )
        .await?;

        if self.no_next {
            let waited = mpv.wait().await;
            self.history.flush();
            return waited;
        }

        // mpv emits start-file for the episode we just launched. Consume it, so
        // the loop below only reacts to *advancing* to a queued episode.
        wait_for_start(&mut mpv).await;

        // Appended but not yet started, oldest first, so a start-file can say
        // which episode the history entry now belongs to — and carry each
        // one's torrent, which becomes the queue trigger's subject when it
        // reaches the screen.
        let mut appended: VecDeque<(Episode, PlayInfo)> = VecDeque::new();
        let mut current = PlayInfo::of(&playable);
        // Set when an episode ends with nothing queued yet (a slow resolve, or
        // a strict trigger): the next one must be fetched now, gap or not.
        let mut force_queue = false;

        // Keep at most one episode queued ahead, and — unlike before — not
        // from the moment playback starts. On a thin pipe the next episode's
        // probe competes with the one on screen for the whole first act, so
        // `queue_when` holds it back until the current file is on disk or
        // playback is far enough along (see `player::QueueWhen`).
        loop {
            if appended.is_empty() && (force_queue || !upcoming.is_empty()) {
                let ready = force_queue
                    || self
                        .queue_when
                        .should_queue(self.history.playing_fraction(), current.downloaded());
                if ready {
                    if let Some(next) = upcoming.front().cloned() {
                        match self.resolve_episode(&show, &next).await {
                            Ok(p) => {
                                self.out.event(&format!("⏭  queued {next}"), || {
                                    json!({
                                        "event": "queued",
                                        "season": next.season,
                                        "episode": next.number,
                                        "name": next.name,
                                    })
                                });
                                mpv.append(&p.url, &p.title).await?;
                                upcoming.pop_front();
                                appended.push_back((next, PlayInfo::of(&p)));
                                force_queue = false;
                            }
                            Err(e) => {
                                self.out.status(&format!("⚠  skipping {next}: {e}"));
                                upcoming.pop_front();
                                continue;
                            }
                        }
                    } else {
                        force_queue = false;
                    }
                }
            }

            if mpv.has_exited() {
                break;
            }

            // While an episode is deliberately being held back, wake every few
            // seconds to re-ask the trigger; otherwise just wait for mpv.
            let event = if appended.is_empty() && !upcoming.is_empty() {
                match tokio::time::timeout(Duration::from_secs(3), mpv.next_event()).await {
                    Ok(event) => event,
                    Err(_) => continue,
                }
            } else {
                mpv.next_event().await
            };

            match event {
                None => break,
                // Deferred queueing means mpv can drain its playlist while the
                // next episode is still resolving. `append-play` will restart
                // it, so idle only ends the run when nothing more is coming.
                Some(Event::Idle) => {
                    if appended.is_empty() && upcoming.is_empty() {
                        break;
                    }
                }
                // Advanced to the queued episode — time to watch its buffer,
                // and to point history at what is now on screen.
                Some(Event::StartFile) => {
                    if let Some((playing, info)) = appended.pop_front() {
                        self.history
                            .set_current(watch_for(&show, &playing, &episodes));
                        current = info;
                    }
                }
                Some(Event::EndFile { reason }) if reason == "quit" => break,
                Some(Event::EndFile { .. }) => {
                    if upcoming.is_empty() && appended.is_empty() {
                        break;
                    }
                    if appended.is_empty() {
                        force_queue = true;
                    }
                }
            }
        }

        let waited = mpv.wait().await;
        self.history.flush();
        waited
    }

    async fn resolve_episode(&self, show: &Show, ep: &Episode) -> Result<Playable> {
        let label = if show.year.is_empty() {
            format!("{} — {}", show.name, ep)
        } else {
            format!("{} ({}) — {}", show.name, show.year, ep)
        };
        self.resolve(
            &show.imdb_id,
            MediaType::Tv,
            Some(ep.season),
            Some(ep.number),
            ep.runtime_secs,
            &label,
        )
        .await
    }

    /// Find a release, prove the swarm can sustain it, and return a URL for mpv.
    ///
    /// Candidates are walked best-first and each is measured. The first to clear
    /// its own bitrate with headroom wins. If none does, the fastest one measured
    /// is used anyway with a warning — a stuttering stream beats no stream, but
    /// the user should know which they are getting.
    async fn resolve(
        &self,
        imdb_id: &str,
        media_type: MediaType,
        season: Option<u32>,
        episode: Option<u32>,
        runtime_secs: u32,
        label: &str,
    ) -> Result<Playable> {
        let out = self.out;
        out.status(&format!("Looking up releases for {label}…"));
        let lookup = self
            .sources
            .candidates(imdb_id, media_type.torrentio(), season, episode)
            .await;

        if !lookup.failed.is_empty() {
            out.status(&format!(
                "⚠  {} unreachable — searching without it",
                lookup.failed.join(" and ")
            ));
        }

        let all = lookup.candidates;
        anyhow::ensure!(!all.is_empty(), "no indexer has a release for {label}");

        let dual_available = all
            .iter()
            .filter(|c| c.audio.dual() > dekho::audio::Dual::No)
            .count();
        let c = lookup.counts;
        let found = all.len();
        out.event(
            &format!(
                "{found} releases (Torrentio {}, apibay {}, {} shared) · {dual_available} dual audio",
                c.torrentio, c.apibay, c.shared,
            ),
            || {
                json!({
                    "event": "releases",
                    "found": found,
                    "dual": dual_available,
                    "torrentio": c.torrentio,
                    "apibay": c.apibay,
                    "shared": c.shared,
                })
            },
        );

        let shortlist = pick::shortlist(all, self.filters, runtime_secs);
        anyhow::ensure!(
            !shortlist.is_empty(),
            "none of the {found} releases for {label} fit the filters{}",
            if self.filters.dual == DualPreference::Only {
                " — no dual-audio release exists for this title, so drop --dual-only or use --dual"
                    .to_string()
            } else if let link::Ceiling::Adapted { bps, .. } = self.ceiling {
                format!(
                    " (the link-adapted bitrate ceiling is {} — pass --max-bitrate to override, \
                     or try -q 1080p)",
                    format_bps(bps)
                )
            } else {
                " (try a higher --max-bitrate, a lower --min-seeders, or -q 1080p)".to_string()
            }
        );

        // The release that played last time goes first: its pieces may still
        // be on disk, and the probe recognises a cached slab and starts
        // instantly instead of measuring.
        let recall_key = torrentio::stream_id(imdb_id, season, episode);
        let remembered = self.replay.recall(&recall_key);
        let order = replay::promote(
            pick::attempt_order(&shortlist, MAX_ATTEMPTS),
            remembered.as_deref(),
        );
        if let Some(hash) = &remembered {
            if order
                .first()
                .map(|c| sources::info_hash_of(&c.magnet) == *hash)
                .unwrap_or(false)
            {
                out.status("Trying the release that played last time first — its pieces may still be cached.");
            }
        }

        // Best fallback seen so far, in case nothing clears the gate.
        let mut fallback: Option<(u64, String, Playable)> = None;
        // The best network rate any probe measured, fed back into the link
        // estimate whatever happens — failed attempts are measurements too.
        let mut best_rate: u64 = 0;

        for candidate in order {
            let needed = candidate.required_bps(runtime_secs);
            let audio = candidate.audio.label();
            let size = candidate.size_bytes.map(format_bytes);
            out.event(
                &format!(
                    "Trying {} · {} · {} seeders{}{}",
                    candidate.quality.label(),
                    size.clone().unwrap_or_else(|| "size unknown".into()),
                    candidate.seeders,
                    if audio.is_empty() {
                        String::new()
                    } else {
                        format!(" · {audio}")
                    },
                    needed
                        .map(|b| format!(" · needs {}", format_bps(b)))
                        .unwrap_or_default(),
                ),
                || {
                    json!({
                        "event": "trying",
                        "quality": candidate.quality.label(),
                        "size": size.unwrap_or_default(),
                        "seeders": candidate.seeders,
                        "audio": audio,
                        "needed_bps": needed,
                    })
                },
            );

            let added = match self.engine.add(&candidate.magnet).await {
                Ok(a) => a,
                Err(e) => {
                    out.event(
                        &format!("   could not add: {e}"),
                        || json!({"event": "downgrade", "reason": "could not add", "rate_bps": 0}),
                    );
                    continue;
                }
            };
            let file_idx = match engine::choose_file(
                &added.files,
                candidate.file_idx,
                season,
                episode,
            ) {
                Ok(i) => i,
                Err(e) => {
                    out.event(&format!("   no playable file: {e}"), || {
                            json!({"event": "downgrade", "reason": "no playable file", "rate_bps": 0})
                        });
                    self.engine.forget(added.id).await;
                    continue;
                }
            };
            // Season packs otherwise fetch every episode at once, splitting
            // bandwidth away from the one on screen.
            self.engine.only_file(&added.handle, file_idx).await;

            let url = self.engine.stream_url(added.id, file_idx);
            let file_name = added
                .files
                .iter()
                .find(|f| f.idx == file_idx)
                .map(|f| f.name.clone())
                .unwrap_or_else(|| candidate.title.clone());
            let file_len = added
                .files
                .iter()
                .find(|f| f.idx == file_idx)
                .map(|f| f.len)
                .unwrap_or(0);
            let entry_name = cache::top_component(&file_name);
            let info_hash = sources::info_hash_of(&candidate.magnet);

            let probe = self
                .engine
                .probe(&added, file_idx, &url, needed, self.filters.link_bps, |s| {
                    out.tick(
                        &format!(
                            "   buffering {} · {} · {} peer{} ({} known)",
                            format_bytes(s.buffered),
                            format_bps(s.rate_bps),
                            s.live_peers,
                            if s.live_peers == 1 { "" } else { "s" },
                            s.seen_peers,
                        ),
                        || {
                            json!({
                                "event": "buffer",
                                "buffered": s.buffered,
                                "rate_bps": s.rate_bps,
                                "live_peers": s.live_peers,
                                "seen_peers": s.seen_peers,
                                "needed_bps": needed,
                            })
                        },
                    );
                })
                .await;

            out.clear();

            match probe {
                Ok(Probe::Ready {
                    rate_bps,
                    buffered,
                    from_cache,
                }) => {
                    out.event(
                        &if from_cache {
                            format!(
                                "   ready · {} already on disk — starting instantly",
                                format_bytes(buffered),
                            )
                        } else {
                            format!(
                                "   ready · {} buffered at {}",
                                format_bytes(buffered),
                                format_bps(rate_bps)
                            )
                        },
                        || {
                            json!({
                                "event": "ready",
                                "rate_bps": rate_bps,
                                "buffered": buffered,
                                "cached": from_cache,
                            })
                        },
                    );
                    // This one won; stop any earlier also-ran from stealing
                    // bandwidth from it.
                    if let Some((_, _, old)) = fallback {
                        self.engine.forget(old.torrent_id).await;
                    }
                    best_rate = best_rate.max(rate_bps);
                    if best_rate > 0 {
                        self.link.observe(best_rate);
                    }
                    self.replay.remember(&recall_key, &info_hash);
                    if let Some(name) = &entry_name {
                        cache::touch(self.download_dir, name);
                    }
                    return Ok(Playable {
                        url,
                        title: label.to_string(),
                        bitrate: needed,
                        torrent_id: added.id,
                        handle: added.handle.clone(),
                        file_idx,
                        file_len,
                        entry_name,
                    });
                }
                Ok(Probe::TooSlow {
                    rate_bps,
                    buffered,
                    live_peers,
                    seen_peers,
                }) => {
                    out.event(
                        &format!(
                            "   too slow · {} sustained, {} buffered, {live_peers} peers \
                             ({seen_peers} known) — trying a lighter release",
                            format_bps(rate_bps),
                            format_bytes(buffered),
                        ),
                        || {
                            json!({
                                "event": "downgrade",
                                "reason": "too slow",
                                "rate_bps": rate_bps,
                            })
                        },
                    );
                    best_rate = best_rate.max(rate_bps);
                    let better = fallback
                        .as_ref()
                        .map(|(r, _, _)| rate_bps > *r)
                        .unwrap_or(true);
                    if better {
                        // Keep the previous fallback's torrent from competing for
                        // bandwidth with everything that comes after it.
                        if let Some((_, _, old)) = fallback.replace((
                            rate_bps,
                            info_hash,
                            Playable {
                                url,
                                title: format!("{label} [{file_name}]"),
                                bitrate: needed,
                                torrent_id: added.id,
                                handle: added.handle.clone(),
                                file_idx,
                                file_len,
                                entry_name,
                            },
                        )) {
                            self.engine.forget(old.torrent_id).await;
                        }
                    } else {
                        self.engine.forget(added.id).await;
                    }
                }
                Err(e) => {
                    out.event(
                        &format!("   probe failed: {e}"),
                        || json!({"event": "downgrade", "reason": "probe failed", "rate_bps": 0}),
                    );
                    self.engine.forget(added.id).await;
                }
            }
        }

        // Whatever happened, the probes were measurements of this link.
        if best_rate > 0 {
            self.link.observe(best_rate);
        }

        match fallback {
            Some((rate, info_hash, playable)) => {
                out.status(&format!(
                    "⚠  No release cleared the smoothness check. Playing the fastest one \
                     ({} sustained) — it may buffer.",
                    format_bps(rate)
                ));
                self.replay.remember(&recall_key, &info_hash);
                if let Some(name) = &playable.entry_name {
                    cache::touch(self.download_dir, name);
                }
                Ok(playable)
            }
            None => anyhow::bail!(
                "could not stream {label}: no release could be started. \
                 Try -q 1080p or --min-seeders 1."
            ),
        }
    }
}

/// The history entry an episode should write, including where "continue" goes
/// once this one is finished.
fn watch_for(show: &Show, ep: &Episode, season: &[Episode]) -> Watch {
    Watch {
        id: show.tmdb_id,
        kind: "tv",
        title: show.name.clone(),
        year: show.year.clone(),
        poster: show.poster.clone(),
        backdrop: show.backdrop.clone(),
        season: Some(ep.season),
        episode: Some(ep.number),
        episode_name: Some(ep.name.clone()),
        next: season
            .iter()
            .find(|e| e.number > ep.number)
            .map(|e| (e.number, e.name.clone())),
    }
}

/// Consume events until the first `start-file`, so the caller's loop only sees
/// transitions between queued entries. Bounded, so a silent mpv cannot hang us.
async fn wait_for_start(mpv: &mut Mpv) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return;
        }
        match tokio::time::timeout(left, mpv.next_event()).await {
            Ok(Some(Event::StartFile)) | Ok(None) | Err(_) => return,
            Ok(Some(_)) => continue,
        }
    }
}

fn choose<T: std::fmt::Display>(prompt: &str, options: Vec<T>) -> Result<T> {
    choose_at(prompt, options, 0, None)
}

/// A picker with an explicit starting position and help line.
///
/// `cursor` matters where the first row is a control rather than a result: the
/// catalog list puts the filters row at the top so it is visible, and starting
/// the cursor below it keeps a quick Enter on "play this" instead of "open a
/// menu".
fn choose_at<T: std::fmt::Display>(
    prompt: &str,
    options: Vec<T>,
    cursor: usize,
    help: Option<&str>,
) -> Result<T> {
    use inquire::error::InquireError;
    let cursor = cursor.min(options.len().saturating_sub(1));
    let mut select = inquire::Select::new(prompt, options)
        .with_page_size(15)
        .with_starting_cursor(cursor);
    if let Some(h) = help {
        select = select.with_help_message(h);
    }
    match select.prompt() {
        Ok(v) => Ok(v),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
            std::process::exit(130)
        }
        Err(e) => Err(e).context("reading your selection"),
    }
}

fn default_download_dir() -> PathBuf {
    xdg::cache_home().join("dekho")
}

/// A durable status line. Goes to stderr so stdout stays clean.
fn status(msg: &str) {
    clear_progress();
    eprintln!("{msg}");
}

/// A transient, overwritten progress line.
fn progress(msg: &str) {
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\x1b[2K{msg}");
    let _ = err.flush();
}

fn clear_progress() {
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\x1b[2K");
    let _ = err.flush();
}
