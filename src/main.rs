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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

use dekho::browse::{self, Kind, Sort};
use dekho::engine::{self, Engine, Probe};
use dekho::history::{self, Recorder, Watch};
use dekho::pick::{self, DualPreference, Filters, MAX_ATTEMPTS};
use dekho::player::{Event, Mpv, ProgressSink};
use dekho::sources::Sources;
use dekho::tmdb::{Episode, MediaType, SearchHit, Show, Tmdb};
use dekho::torrentio::{format_bps, format_bytes, Quality};
use dekho::{api, config, xdg};

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
    /// separates a streamable 4K WEB-DL from an unstreamable 4K remux.
    #[arg(long, default_value_t = 40, global = true)]
    max_bitrate: u64,

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
}

#[derive(Subcommand)]
enum Command {
    /// Browse the catalog with filters and sorting, then play what you pick
    Browse(BrowseArgs),
    /// Play a title by TMDB id without asking anything
    Play(PlayArgs),
    /// Answer catalog and history questions as JSON, for panels and scripts
    Api(ApiArgs),
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
struct ApiArgs {
    #[command(subcommand)]
    verb: ApiVerb,
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
    /// Everything about one title, seasons included
    Title {
        #[arg(long)]
        id: u32,
        /// `movie` or `tv`
        #[arg(long)]
        kind: String,
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
    /// Download TMDB images into the local cache and report their paths
    Prefetch {
        /// w92, w154, w185, w342, w500, w780 or original
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

    let max_quality = Quality::parse_cap(&cli.quality)
        .with_context(|| format!("unknown --quality {:?}; try 720p, 1080p or 4k", cli.quality))?;
    let filters = Filters {
        max_quality,
        max_bitrate: cli.max_bitrate.saturating_mul(1_000_000),
        min_seeders: cli.min_seeders,
        dual: match (cli.dual, cli.dual_only) {
            (_, true) => DualPreference::Only,
            (true, _) => DualPreference::Prefer,
            _ => DualPreference::Ignore,
        },
    };

    let out = Out {
        json: matches!(&cli.command, Some(Command::Play(p)) if p.json),
    };
    let result = run(&cli, &filters, &out).await;
    out.finish(result)
}

async fn run(cli: &Cli, filters: &Filters, out: &Out) -> Result<()> {
    let tmdb = Tmdb::new(config::tmdb_key()?)?;
    let sources = Sources::new()?;

    // Resolve what to watch before booting the engine, so a typo or an empty
    // catalog costs nothing.
    let mut target: Option<(u32, MediaType)> = None;
    let mut browse_state: Option<(browse::Filters, u32)> = None;

    match &cli.command {
        Some(Command::Api(_)) => unreachable!("answered before the engine boots"),
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
    let engine = Engine::start(download_dir).await?;

    let run = Run {
        tmdb: &tmdb,
        sources: &sources,
        engine: &engine,
        filters,
        out,
        history: Recorder::new(history::path()),
        season: cli.season,
        episode: cli.episode,
        no_next: cli.no_next,
        resume: cli.resume,
        dry_run: cli.dry_run,
        interactive: !matches!(cli.command, Some(Command::Play(_))),
    };

    match (target, browse_state) {
        (Some((id, kind)), _) => run.play(id, kind).await,
        (None, Some((bf, page))) => browse_loop(&run, bf, page).await,
        (None, None) => unreachable!("one of the two branches above always applies"),
    }
}

/// Run one `api` verb and print its object. Failure is an object too — a caller
/// must never have to read stderr to find out what went wrong.
async fn run_api(args: &ApiArgs, cli: &Cli) -> Result<()> {
    // Write errors are dropped rather than reported: piping into `head` closes
    // the pipe, and a broken-pipe complaint on stderr would be the one thing on
    // stderr this command ever prints.
    let mut out = std::io::stdout();
    match api_value(&args.verb, cli).await {
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
            page,
        } => {
            let filters = browse::filters_from(
                api_kind(kind)?,
                sort.as_deref(),
                genre.as_deref(),
                lang.as_deref(),
                *min_rating,
            )?;
            api::discover(&tmdb()?, &filters, *page).await
        }
        ApiVerb::Genres { kind } => Ok(api::genres(api_kind(kind)?)),
        ApiVerb::Languages => Ok(api::languages()),
        ApiVerb::Title { id, kind } => {
            api::title(&tmdb()?, *id, MediaType::of(api_kind(kind)?)).await
        }
        ApiVerb::Episodes { id } => {
            let season = cli.season.context("api episodes needs --season")?;
            api::episodes(&tmdb()?, *id, season).await
        }
        ApiVerb::History { limit } => api::history(*limit),
        ApiVerb::Prefetch { size, paths } => api::prefetch(size, paths).await,
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
    )
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
            playable.bitrate,
            start,
            self.sink(),
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
            playable.bitrate,
            start,
            self.sink(),
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
        // which episode the history entry now belongs to.
        let mut appended: VecDeque<Episode> = VecDeque::new();

        // Keep exactly one episode queued ahead. Appending earlier would start
        // extra torrents that compete for bandwidth with the one being watched;
        // appending later would leave a gap at the episode boundary.
        loop {
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
                        appended.push_back(next);
                    }
                    Err(e) => {
                        self.out.status(&format!("⚠  skipping {next}: {e}"));
                        upcoming.pop_front();
                        continue;
                    }
                }
            }

            if mpv.has_exited() {
                break;
            }

            match mpv.next_event().await {
                None => break,
                Some(Event::Idle) => break,
                // Advanced to the queued episode — time to prepare the next one,
                // and to point history at what is now on screen.
                Some(Event::StartFile) => {
                    if let Some(playing) = appended.pop_front() {
                        self.history
                            .set_current(watch_for(&show, &playing, &episodes));
                    }
                    continue;
                }
                Some(Event::EndFile { reason }) if reason == "quit" => break,
                Some(Event::EndFile { .. }) => {
                    if upcoming.is_empty() {
                        break;
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
            } else {
                " (try a higher --max-bitrate, a lower --min-seeders, or -q 1080p)"
            }
        );

        // Best fallback seen so far, in case nothing clears the gate.
        let mut fallback: Option<(u64, Playable)> = None;

        for candidate in pick::attempt_order(&shortlist, MAX_ATTEMPTS) {
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

            let probe = self
                .engine
                .probe(&added, file_idx, &url, needed, |s| {
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
                Ok(Probe::Ready { rate_bps, buffered }) => {
                    out.event(
                        &format!(
                            "   ready · {} buffered at {}",
                            format_bytes(buffered),
                            format_bps(rate_bps)
                        ),
                        || json!({"event": "ready", "rate_bps": rate_bps, "buffered": buffered}),
                    );
                    // This one won; stop any earlier also-ran from stealing
                    // bandwidth from it.
                    if let Some((_, old)) = fallback {
                        self.engine.forget(old.torrent_id).await;
                    }
                    return Ok(Playable {
                        url,
                        title: label.to_string(),
                        bitrate: needed,
                        torrent_id: added.id,
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
                    let better = fallback
                        .as_ref()
                        .map(|(r, _)| rate_bps > *r)
                        .unwrap_or(true);
                    if better {
                        // Keep the previous fallback's torrent from competing for
                        // bandwidth with everything that comes after it.
                        if let Some((_, old)) = fallback.replace((
                            rate_bps,
                            Playable {
                                url,
                                title: format!("{label} [{file_name}]"),
                                bitrate: needed,
                                torrent_id: added.id,
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

        match fallback {
            Some((rate, playable)) => {
                out.status(&format!(
                    "⚠  No release cleared the smoothness check. Playing the fastest one \
                     ({} sustained) — it may buffer.",
                    format_bps(rate)
                ));
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
