//! dekho — search a movie or series and stream it straight into mpv.
//!
//! Nothing here talks to kojev.com. Metadata comes from TMDB with your own key,
//! releases come from Torrentio and apibay together (see `sources`), and
//! playback is a local torrent stream handed to mpv as an ordinary HTTP URL.
//!
//! The design goal that shapes everything is *no buffering*. See `pick` for how
//! a release is chosen, `engine` for the throughput gate that vetoes one the
//! swarm cannot sustain, and `player` for mpv's own cushion on top.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use dekho::browse::{self, Kind, Sort};
use dekho::engine::{self, Engine, Probe};
use dekho::pick::{self, DualPreference, Filters, MAX_ATTEMPTS};
use dekho::player::{Event, Mpv};
use dekho::sources::Sources;
use dekho::tmdb::{Episode, MediaType, SearchHit, Show, Tmdb};
use dekho::torrentio::{format_bps, format_bytes, Quality};

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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

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

    let key = std::env::var("TMDB_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .context(
            "TMDB_API_KEY is not set.\n\
             Get a free key at https://www.themoviedb.org/settings/api and export it:\n  \
             set -Ux TMDB_API_KEY <your key>",
        )?;

    let tmdb = Tmdb::new(key)?;
    let sources = Sources::new()?;

    // Resolve what to watch before booting the engine, so a typo or an empty
    // catalog costs nothing.
    let browse_state = match &cli.command {
        Some(Command::Browse(args)) => {
            let bf = initial_filters(args)?;
            if args.list {
                return print_page(&tmdb, &bf, args.page).await;
            }
            Some((bf, args.page.max(1)))
        }
        None => None,
    };

    let hit = match &browse_state {
        None => {
            let query = cli.query.join(" ");
            anyhow::ensure!(
                !query.trim().is_empty(),
                "nothing to search for — try `dekho fight club` or `dekho browse`"
            );
            status(&format!("Searching TMDB for {query:?}…"));
            let mut hits = tmdb.search(&query).await?;
            anyhow::ensure!(!hits.is_empty(), "nothing on TMDB matched {query:?}");
            if cli.first {
                let top = hits.remove(0);
                status(&format!("→  {top}"));
                Some(top)
            } else {
                Some(choose("What do you want to watch?", hits)?)
            }
        }
        Some(_) => None,
    };

    // --- boot the engine ----------------------------------------------------
    let download_dir = cli
        .download_dir
        .clone()
        .unwrap_or_else(default_download_dir);
    status(&format!("Cache: {}", download_dir.display()));
    let engine = Engine::start(download_dir).await?;

    match (hit, browse_state) {
        (Some(hit), _) => play(&tmdb, &sources, &engine, &filters, &hit, &cli).await,
        (None, Some((bf, page))) => {
            browse_loop(&tmdb, &sources, &engine, &filters, &cli, bf, page).await
        }
        (None, None) => unreachable!("one of the two branches above always applies"),
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

async fn play(
    tmdb: &Tmdb,
    sources: &Sources,
    engine: &Engine,
    filters: &Filters,
    hit: &SearchHit,
    cli: &Cli,
) -> Result<()> {
    match hit.media_type {
        MediaType::Movie => play_movie(tmdb, sources, engine, filters, hit, cli).await,
        MediaType::Tv => play_series(tmdb, sources, engine, filters, hit, cli).await,
    }
}

/// Turn `browse` flags into a starting filter set, rejecting bad values up
/// front rather than silently ignoring them.
fn initial_filters(args: &BrowseArgs) -> Result<browse::Filters> {
    let kind = match args.kind.as_deref().map(str::trim) {
        None => choose("What are you in the mood for?", vec![Kind::Movie, Kind::Tv])?,
        Some(k) => match k.to_ascii_lowercase().as_str() {
            "movie" | "movies" | "film" | "films" => Kind::Movie,
            "tv" | "series" | "shows" | "show" => Kind::Tv,
            other => anyhow::bail!("unknown kind {other:?}; use `movies` or `tv`"),
        },
    };

    let mut f = browse::Filters::new(kind);

    if let Some(s) = &args.sort {
        let sort = Sort::parse(s).with_context(|| {
            format!("unknown --sort {s:?}; try popular, top-rated, newest, oldest, box-office")
        })?;
        anyhow::ensure!(
            Sort::all(kind).contains(&sort),
            "--sort box-office only applies to movies"
        );
        f.sort = sort;
    }
    if let Some(g) = &args.genre {
        f.genre_id = browse::parse_genre(kind, g).with_context(|| {
            let names: Vec<&str> = browse::genres_for(kind).iter().map(|(_, n)| *n).collect();
            format!(
                "unknown or ambiguous --genre {g:?}. Options: {}",
                names.join(", ")
            )
        })?;
    }
    if let Some(l) = &args.lang {
        f.language = browse::parse_language(l)
            .with_context(|| {
                format!("unknown --lang {l:?}; try a code like `bn` or a name like `Bangla`")
            })?
            .to_string();
    }
    if let Some(r) = args.min_rating {
        anyhow::ensure!((5..=9).contains(&r), "--min-rating must be between 5 and 9");
        f.min_rating = r;
    }
    Ok(f)
}

/// The catalog browser: page through results, adjust filters, play a pick, and
/// come back to the same place afterwards.
async fn browse_loop(
    tmdb: &Tmdb,
    sources: &Sources,
    engine: &Engine,
    filters: &Filters,
    cli: &Cli,
    mut bf: browse::Filters,
    start_page: u32,
) -> Result<()> {
    let mut page: u32 = start_page.max(1);

    loop {
        status(&format!("Loading {} · {}…", bf.kind, bf.summary()));
        let catalog = tmdb.discover(&bf, page).await?;

        if catalog.items.is_empty() {
            status("Nothing matched those filters.");
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
                play(tmdb, sources, engine, filters, &hit, cli).await?;
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

/// Report what would play and stop, for `--dry-run`.
fn report_dry_run(playable: &Playable) -> Result<()> {
    println!("would play: {}", playable.title);
    println!("stream:     {}", playable.url);
    match playable.bitrate {
        Some(b) => println!("bitrate:    {}", format_bps(b)),
        None => println!("bitrate:    unknown"),
    }
    Ok(())
}

async fn play_movie(
    tmdb: &Tmdb,
    sources: &Sources,
    engine: &Engine,
    filters: &Filters,
    hit: &SearchHit,
    cli: &Cli,
) -> Result<()> {
    let movie = tmdb.movie(hit.id).await?;
    let label = if movie.year.is_empty() {
        movie.title.clone()
    } else {
        format!("{} ({})", movie.title, movie.year)
    };

    let playable = resolve(
        sources,
        engine,
        filters,
        &movie.imdb_id,
        MediaType::Movie,
        None,
        None,
        movie.runtime_secs,
        &label,
    )
    .await?;

    if cli.dry_run {
        return report_dry_run(&playable);
    }

    status(&format!("▶  {label}"));
    let mut mpv = Mpv::launch(&playable.url, &playable.title, playable.bitrate).await?;
    mpv.wait().await
}

async fn play_series(
    tmdb: &Tmdb,
    sources: &Sources,
    engine: &Engine,
    filters: &Filters,
    hit: &SearchHit,
    cli: &Cli,
) -> Result<()> {
    let show = tmdb.show(hit.id).await?;

    let season = match cli.season {
        Some(n) => *show
            .seasons
            .iter()
            .find(|s| s.number == n)
            .with_context(|| format!("{} has no season {n}", show.name))?,
        None if show.seasons.len() == 1 => show.seasons[0],
        None => choose("Which season?", show.seasons.clone())?,
    };

    let episodes = tmdb.episodes(&show, season.number).await?;
    anyhow::ensure!(
        !episodes.is_empty(),
        "TMDB lists no episodes for season {}",
        season.number
    );

    let start_at = match cli.episode {
        Some(n) => episodes
            .iter()
            .position(|e| e.number == n)
            .with_context(|| format!("season {} has no episode {n}", season.number))?,
        None => {
            let chosen = choose("Which episode?", episodes.clone())?;
            episodes
                .iter()
                .position(|e| e.number == chosen.number)
                .unwrap_or(0)
        }
    };

    // Everything from the chosen episode onward, so playback keeps going.
    let mut upcoming: VecDeque<Episode> = episodes.into_iter().skip(start_at).collect();
    let first = upcoming.pop_front().context("no episode to play")?;

    let playable = resolve_episode(sources, engine, filters, &show, &first).await?;

    if cli.dry_run {
        report_dry_run(&playable)?;
        println!(
            "then:       {}",
            upcoming
                .front()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "(end of season)".into())
        );
        return Ok(());
    }

    status(&format!("▶  {} — {}", show.name, first));
    let mut mpv = Mpv::launch(&playable.url, &playable.title, playable.bitrate).await?;

    if cli.no_next {
        return mpv.wait().await;
    }

    // mpv emits start-file for the episode we just launched. Consume it, so the
    // loop below only reacts to *advancing* to a queued episode.
    wait_for_start(&mut mpv).await;

    // Keep exactly one episode queued ahead. Appending earlier would start
    // extra torrents that compete for bandwidth with the one being watched;
    // appending later would leave a gap at the episode boundary.
    loop {
        if let Some(next) = upcoming.front().cloned() {
            match resolve_episode(sources, engine, filters, &show, &next).await {
                Ok(p) => {
                    status(&format!("⏭  queued {next}"));
                    mpv.append(&p.url, &p.title).await?;
                    upcoming.pop_front();
                }
                Err(e) => {
                    status(&format!("⚠  skipping {next}: {e}"));
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
            // Advanced to the queued episode — time to prepare the next one.
            Some(Event::StartFile) => continue,
            Some(Event::EndFile { reason }) if reason == "quit" => break,
            Some(Event::EndFile { .. }) => {
                if upcoming.is_empty() {
                    break;
                }
            }
        }
    }

    mpv.wait().await
}

async fn resolve_episode(
    sources: &Sources,
    engine: &Engine,
    filters: &Filters,
    show: &Show,
    ep: &Episode,
) -> Result<Playable> {
    let label = if show.year.is_empty() {
        format!("{} — {}", show.name, ep)
    } else {
        format!("{} ({}) — {}", show.name, show.year, ep)
    };
    resolve(
        sources,
        engine,
        filters,
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
/// is used anyway with a warning — a stuttering stream beats no stream, but the
/// user should know which they are getting.
#[allow(clippy::too_many_arguments)]
async fn resolve(
    sources: &Sources,
    engine: &Engine,
    filters: &Filters,
    imdb_id: &str,
    media_type: MediaType,
    season: Option<u32>,
    episode: Option<u32>,
    runtime_secs: u32,
    label: &str,
) -> Result<Playable> {
    status(&format!("Looking up releases for {label}…"));
    let lookup = sources
        .candidates(imdb_id, media_type.torrentio(), season, episode)
        .await;

    if !lookup.failed.is_empty() {
        status(&format!(
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
    status(&format!(
        "{} releases (Torrentio {}, apibay {}, {} shared) · {dual_available} dual audio",
        all.len(),
        c.torrentio,
        c.apibay,
        c.shared,
    ));

    let found = all.len();
    let shortlist = pick::shortlist(all, filters, runtime_secs);
    anyhow::ensure!(
        !shortlist.is_empty(),
        "none of the {found} releases for {label} fit the filters{}",
        if filters.dual == DualPreference::Only {
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
        status(&format!(
            "Trying {} · {} · {} seeders{}{}",
            candidate.quality.label(),
            candidate
                .size_bytes
                .map(format_bytes)
                .unwrap_or_else(|| "size unknown".into()),
            candidate.seeders,
            if audio.is_empty() {
                String::new()
            } else {
                format!(" · {audio}")
            },
            needed
                .map(|b| format!(" · needs {}", format_bps(b)))
                .unwrap_or_default(),
        ));

        let added = match engine.add(&candidate.magnet).await {
            Ok(a) => a,
            Err(e) => {
                status(&format!("   could not add: {e}"));
                continue;
            }
        };
        let file_idx = match engine::choose_file(&added.files, candidate.file_idx, season, episode)
        {
            Ok(i) => i,
            Err(e) => {
                status(&format!("   no playable file: {e}"));
                engine.forget(added.id).await;
                continue;
            }
        };
        // Season packs otherwise fetch every episode at once, splitting
        // bandwidth away from the one on screen.
        engine.only_file(&added.handle, file_idx).await;

        let url = engine.stream_url(added.id, file_idx);
        let file_name = added
            .files
            .iter()
            .find(|f| f.idx == file_idx)
            .map(|f| f.name.clone())
            .unwrap_or_else(|| candidate.title.clone());

        let probe = engine
            .probe(&added, file_idx, &url, needed, |s| {
                progress(&format!(
                    "   buffering {} · {} · {} peer{} ({} known)",
                    format_bytes(s.buffered),
                    format_bps(s.rate_bps),
                    s.live_peers,
                    if s.live_peers == 1 { "" } else { "s" },
                    s.seen_peers,
                ));
            })
            .await;

        clear_progress();

        match probe {
            Ok(Probe::Ready { rate_bps, buffered }) => {
                status(&format!(
                    "   ready · {} buffered at {}",
                    format_bytes(buffered),
                    format_bps(rate_bps)
                ));
                // This one won; stop any earlier also-ran from stealing
                // bandwidth from it.
                if let Some((_, old)) = fallback {
                    engine.forget(old.torrent_id).await;
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
                status(&format!(
                    "   too slow · {} sustained, {} buffered, {live_peers} peers \
                     ({seen_peers} known) — trying a lighter release",
                    format_bps(rate_bps),
                    format_bytes(buffered),
                ));
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
                        engine.forget(old.torrent_id).await;
                    }
                } else {
                    engine.forget(added.id).await;
                }
            }
            Err(e) => {
                status(&format!("   probe failed: {e}"));
                engine.forget(added.id).await;
            }
        }
    }

    match fallback {
        Some((rate, playable)) => {
            status(&format!(
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
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("dekho");
        }
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => PathBuf::from(home).join(".cache").join("dekho"),
        _ => std::env::temp_dir().join("dekho"),
    }
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
