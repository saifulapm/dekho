//! TMDB client.
//!
//! We talk to TMDB directly with the user's own key rather than going through
//! kojev.com — no dependency on the site, and nothing billed against its
//! Workers free tier.
//!
//! Runtime matters more here than it looks: it is the denominator that turns a
//! release's byte size into a required bitrate, which is what `pick` uses to
//! decide whether a given release can actually be streamed smoothly. When TMDB
//! has no runtime we fall back rather than fail, but the estimate gets softer.

use std::cmp::Ordering;
use std::collections::HashSet;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::browse::{Filters, Kind, Sort};

const BASE: &str = "https://api.themoviedb.org/3";

/// TMDB rejects `page` above this, regardless of the `total_pages` it reports.
const TMDB_MAX_PAGE: u32 = 500;

/// Assumed runtime when TMDB reports none. Deliberately on the long side: a
/// too-long guess understates the bitrate, which makes us *more* willing to try
/// a release, and the live throughput probe still has the final say.
const FALLBACK_MOVIE_MINUTES: u32 = 110;
const FALLBACK_EPISODE_MINUTES: u32 = 45;

/// Caps on the rich half of a title. A panel shows shelves, not phone books,
/// and every extra row is a face to download before it can be drawn.
const MAX_CAST: usize = 20;
const MAX_CREW: usize = 12;
const MAX_SIMILAR: usize = 20;
/// Credits on a person page. A prolific actor has hundreds of them.
const MAX_CREDITS: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MediaType {
    Movie,
    Tv,
}

impl MediaType {
    /// Torrentio's word for it.
    pub fn torrentio(self) -> &'static str {
        match self {
            MediaType::Movie => "movie",
            MediaType::Tv => "series",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MediaType::Movie => "Movie",
            MediaType::Tv => "Series",
        }
    }

    /// TMDB's word for it, which is also what `dekho api` emits.
    pub fn key(self) -> &'static str {
        match self {
            MediaType::Movie => "movie",
            MediaType::Tv => "tv",
        }
    }

    pub fn of(kind: Kind) -> MediaType {
        match kind {
            Kind::Movie => MediaType::Movie,
            Kind::Tv => MediaType::Tv,
        }
    }
}

/// One search hit, already narrowed to something we can actually play.
///
/// The artwork and overview are carried even though the interactive pickers
/// never show them: `dekho api` hands the same hit to a panel that does.
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub id: u32,
    pub media_type: MediaType,
    pub title: String,
    pub year: String,
    pub vote: f64,
    /// TMDB paths, leading slash included, or empty when TMDB has none.
    pub poster: String,
    pub backdrop: String,
    pub overview: String,
}

impl std::fmt::Display for SearchHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let year = if self.year.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.year)
        };
        write!(
            f,
            "{:<7} {}{}  ★ {:.1}",
            self.media_type.label(),
            self.title,
            year,
            self.vote
        )
    }
}

/// One person in a title's cast, in TMDB's own billing order.
#[derive(Clone, Debug)]
pub struct CastMember {
    pub id: u32,
    pub name: String,
    pub character: String,
    /// TMDB path with a leading slash, empty when the person has no photo.
    pub profile: String,
    pub order: u32,
}

/// One person behind a title, in a job a viewer would recognise.
#[derive(Clone, Debug)]
pub struct CrewMember {
    pub id: u32,
    pub name: String,
    pub job: String,
    pub profile: String,
}

/// A video TMDB knows about — in practice a YouTube trailer.
#[derive(Clone, Debug)]
pub struct Video {
    pub key: String,
    pub site: String,
    /// TMDB's `type`: Trailer, Teaser, Clip, Featurette…
    pub kind: String,
    pub name: String,
    pub official: bool,
    /// ISO-8601 timestamp, empty when TMDB has none.
    pub published: String,
}

impl Video {
    /// Something mpv can open. Only YouTube survives `rank_videos`, so this
    /// never has to branch on the site.
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.key)
    }
}

/// Facts only a movie has.
#[derive(Clone, Debug)]
pub struct MovieFacts {
    pub budget: u64,
    pub revenue: u64,
    pub release_date: String,
}

/// Facts only a series has.
#[derive(Clone, Debug)]
pub struct ShowFacts {
    pub season_count: u32,
    pub episode_count: u32,
    pub first_air: String,
    pub last_air: String,
    pub networks: Vec<String>,
    pub in_production: bool,
}

/// The rich half of a title: who made it, what it looks like, what to watch
/// next. Fetched in the same round trip as the rest — `append_to_response`
/// turns what would be five requests into one, which on a thin link is the
/// whole difference between a detail view that opens and one that loads.
#[derive(Clone, Debug, Default)]
pub struct TitleDetail {
    pub tagline: String,
    pub status: String,
    pub homepage: String,
    pub cast: Vec<CastMember>,
    pub crew: Vec<CrewMember>,
    /// Best first; the first entry is what `trailer` plays.
    pub videos: Vec<Video>,
    pub studios: Vec<String>,
    pub countries: Vec<String>,
    pub languages: Vec<String>,
    pub similar: Vec<SearchHit>,
    pub movie: Option<MovieFacts>,
    pub show: Option<ShowFacts>,
}

/// One title on a person's page: an ordinary hit plus what they did in it.
#[derive(Clone, Debug)]
pub struct Credit {
    pub hit: SearchHit,
    /// Empty for a crew credit.
    pub character: String,
    /// Empty for a cast credit.
    pub job: String,
    /// TMDB's own popularity figure, which is what orders the list.
    pub popularity: f64,
    /// A talk-show sofa rather than a role. See `is_appearance`.
    pub appearance: bool,
    /// Carried so a person's TV credits can be filtered the way `discover`
    /// filters the catalog — see `person_page`.
    pub genre_ids: Vec<u32>,
    pub language: String,
}

/// A person, as a cast click resolves them.
#[derive(Clone, Debug)]
pub struct PersonDetail {
    pub id: u32,
    pub name: String,
    pub profile: String,
    pub biography: String,
    /// TMDB's `known_for_department`: Acting, Directing, Writing…
    pub known_for: String,
    pub birthday: Option<String>,
    pub deathday: Option<String>,
    pub place_of_birth: String,
    pub credits: Vec<Credit>,
}

pub struct Tmdb {
    http: reqwest::Client,
    key: String,
}

#[derive(Deserialize, Default)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    id: u32,
    media_type: Option<String>,
    title: Option<String>,
    name: Option<String>,
    release_date: Option<String>,
    first_air_date: Option<String>,
    vote_average: Option<f64>,
    #[serde(default)]
    poster_path: Option<String>,
    #[serde(default)]
    backdrop_path: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    // Only ever set on `combined_credits` entries, which is why they can reuse
    // this struct rather than declaring a near-identical one.
    #[serde(default)]
    popularity: Option<f64>,
    #[serde(default)]
    character: Option<String>,
    #[serde(default)]
    job: Option<String>,
    #[serde(default)]
    genre_ids: Option<Vec<u32>>,
    #[serde(default)]
    original_language: Option<String>,
}

impl SearchItem {
    /// Narrow a raw result to something playable, or drop it.
    ///
    /// `implied` is the kind the endpoint already fixed: `/discover` and
    /// `/trending/movie` results carry no `media_type` of their own. Where
    /// there is none — a multi-search — anything that is not a movie or a show
    /// is dropped here rather than shown in a picker and rejected later.
    fn into_hit(self, implied: Option<MediaType>) -> Option<SearchHit> {
        let media_type = match self.media_type.as_deref() {
            Some("movie") => MediaType::Movie,
            Some("tv") => MediaType::Tv,
            _ => implied?,
        };
        let title = self.title.or(self.name)?;
        if title.trim().is_empty() {
            return None;
        }
        let year = match media_type {
            MediaType::Movie => year_of(&self.release_date),
            MediaType::Tv => year_of(&self.first_air_date),
        };
        Some(SearchHit {
            id: self.id,
            media_type,
            title,
            year,
            vote: self.vote_average.unwrap_or(0.0),
            poster: self.poster_path.unwrap_or_default(),
            backdrop: self.backdrop_path.unwrap_or_default(),
            overview: self.overview.unwrap_or_default(),
        })
    }

    /// A `combined_credits` entry: the same hit, plus what the person did.
    ///
    /// The kind is read off the entry itself — a filmography is the one list
    /// that genuinely mixes films and shows.
    /// `implied` matters for the same reason it does in `into_hit`:
    /// `combined_credits` labels each entry, `tv_credits` does not.
    fn into_credit(self, implied: Option<MediaType>) -> Option<Credit> {
        let character = self.character.clone().unwrap_or_default();
        let job = self.job.clone().unwrap_or_default();
        let popularity = self.popularity.unwrap_or(0.0);
        let genre_ids = self.genre_ids.clone().unwrap_or_default();
        let language = self.original_language.clone().unwrap_or_default();
        let appearance = is_appearance(&character, &genre_ids);
        Some(Credit {
            hit: self.into_hit(implied)?,
            character,
            job,
            popularity,
            appearance,
            genre_ids,
            language,
        })
    }
}

#[derive(Deserialize)]
struct DiscoverResponse {
    results: Vec<SearchItem>,
    total_pages: Option<u32>,
}

/// One page of catalog results.
pub struct CatalogPage {
    pub items: Vec<SearchHit>,
    pub page: u32,
    pub total_pages: u32,
}

impl CatalogPage {
    pub fn has_next(&self) -> bool {
        self.page < self.total_pages
    }
}

#[derive(Deserialize)]
struct MovieDetail {
    imdb_id: Option<String>,
    title: Option<String>,
    original_title: Option<String>,
    runtime: Option<u32>,
    release_date: Option<String>,
    vote_average: Option<f64>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    overview: Option<String>,
    genres: Option<Vec<GenreRef>>,
    // --- only populated when `append_to_response` asked for them -----------
    tagline: Option<String>,
    status: Option<String>,
    homepage: Option<String>,
    budget: Option<u64>,
    revenue: Option<u64>,
    production_companies: Option<Vec<NamedRef>>,
    production_countries: Option<Vec<CountryRef>>,
    spoken_languages: Option<Vec<LanguageRef>>,
    credits: Option<CreditsResponse>,
    videos: Option<VideoResponse>,
    similar: Option<SearchResponse>,
}

#[derive(Deserialize)]
struct GenreRef {
    name: Option<String>,
}

/// `production_companies`, `networks` — anything TMDB models as a named thing.
#[derive(Deserialize)]
struct NamedRef {
    name: Option<String>,
}

#[derive(Deserialize)]
struct CountryRef {
    iso_3166_1: Option<String>,
}

#[derive(Deserialize)]
struct LanguageRef {
    english_name: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct CreatedByRef {
    id: u32,
    name: Option<String>,
    profile_path: Option<String>,
}

#[derive(Deserialize, Default)]
struct CreditsResponse {
    #[serde(default)]
    cast: Vec<CastItem>,
    #[serde(default)]
    crew: Vec<CrewItem>,
}

#[derive(Deserialize)]
struct CastItem {
    id: u32,
    name: Option<String>,
    character: Option<String>,
    profile_path: Option<String>,
    order: Option<u32>,
}

#[derive(Deserialize)]
struct CrewItem {
    id: u32,
    name: Option<String>,
    job: Option<String>,
    profile_path: Option<String>,
}

#[derive(Deserialize, Default)]
struct VideoResponse {
    #[serde(default)]
    results: Vec<VideoItem>,
}

#[derive(Deserialize)]
struct VideoItem {
    key: Option<String>,
    site: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    official: Option<bool>,
    published_at: Option<String>,
}

#[derive(Deserialize)]
struct PersonResponse {
    id: u32,
    name: Option<String>,
    profile_path: Option<String>,
    biography: Option<String>,
    known_for_department: Option<String>,
    birthday: Option<String>,
    deathday: Option<String>,
    place_of_birth: Option<String>,
    combined_credits: Option<SearchResponseWithCrew>,
}

/// `combined_credits` is two lists of the same shape: what they acted in and
/// what they worked on.
#[derive(Deserialize, Default)]
struct SearchResponseWithCrew {
    #[serde(default)]
    cast: Vec<SearchItem>,
    #[serde(default)]
    crew: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct ExternalIds {
    imdb_id: Option<String>,
}

#[derive(Deserialize)]
struct ShowDetail {
    name: Option<String>,
    original_name: Option<String>,
    first_air_date: Option<String>,
    episode_run_time: Option<Vec<u32>>,
    seasons: Option<Vec<SeasonSummary>>,
    external_ids: Option<ExternalIds>,
    vote_average: Option<f64>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    overview: Option<String>,
    genres: Option<Vec<GenreRef>>,
    // --- only populated when `append_to_response` asked for them -----------
    tagline: Option<String>,
    status: Option<String>,
    homepage: Option<String>,
    number_of_seasons: Option<u32>,
    number_of_episodes: Option<u32>,
    last_air_date: Option<String>,
    in_production: Option<bool>,
    networks: Option<Vec<NamedRef>>,
    origin_country: Option<Vec<String>>,
    created_by: Option<Vec<CreatedByRef>>,
    production_companies: Option<Vec<NamedRef>>,
    production_countries: Option<Vec<CountryRef>>,
    spoken_languages: Option<Vec<LanguageRef>>,
    credits: Option<CreditsResponse>,
    videos: Option<VideoResponse>,
    similar: Option<SearchResponse>,
}

#[derive(Deserialize)]
struct SeasonSummary {
    season_number: u32,
    episode_count: Option<u32>,
    name: Option<String>,
    poster_path: Option<String>,
    air_date: Option<String>,
}

#[derive(Deserialize)]
struct SeasonDetail {
    episodes: Option<Vec<EpisodeDetail>>,
}

#[derive(Deserialize)]
struct EpisodeDetail {
    episode_number: u32,
    name: Option<String>,
    runtime: Option<u32>,
    overview: Option<String>,
    still_path: Option<String>,
    air_date: Option<String>,
    vote_average: Option<f64>,
}

/// A movie, resolved far enough to look up torrents for it.
#[derive(Clone, Debug)]
pub struct Movie {
    pub id: u32,
    pub title: String,
    /// The title in the film's own language — `La casa de papel` where `title`
    /// says `Money Heist`. Releases are named after either.
    pub original_title: String,
    pub year: String,
    /// Empty when TMDB has no IMDB id, which means no torrents can be found.
    pub imdb_id: String,
    pub runtime_secs: u32,
    pub vote: f64,
    pub poster: String,
    pub backdrop: String,
    pub overview: String,
    pub genres: Vec<String>,
}

/// A show, resolved far enough to enumerate and look up its episodes.
#[derive(Clone, Debug)]
pub struct Show {
    pub tmdb_id: u32,
    pub name: String,
    /// The name in the show's own language — see `Movie::original_title`.
    pub original_name: String,
    pub year: String,
    /// Empty when TMDB has no IMDB id, which means no torrents can be found.
    pub imdb_id: String,
    /// Seasons with at least one episode, specials (season 0) excluded.
    pub seasons: Vec<Season>,
    /// Show-level typical runtime, used when an episode has none of its own.
    pub default_runtime_secs: u32,
    pub vote: f64,
    pub poster: String,
    pub backdrop: String,
    pub overview: String,
    pub genres: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Season {
    pub number: u32,
    pub episode_count: u32,
    pub name: String,
    pub poster: String,
    pub year: String,
}

impl std::fmt::Display for Season {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Season {} ({} episode{})",
            self.number,
            self.episode_count,
            if self.episode_count == 1 { "" } else { "s" }
        )
    }
}

#[derive(Clone, Debug)]
pub struct Episode {
    pub season: u32,
    pub number: u32,
    pub name: String,
    pub runtime_secs: u32,
    pub overview: String,
    pub still: String,
    pub air_date: String,
    pub vote: f64,
}

impl std::fmt::Display for Episode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "S{:02}E{:02}  {}", self.season, self.number, self.name)
    }
}

fn year_of(date: &Option<String>) -> String {
    date.as_deref()
        .and_then(|d| d.get(..4))
        .unwrap_or("")
        .to_string()
}

impl Tmdb {
    pub fn new(key: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("dekho/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("building the TMDB HTTP client")?;
        Ok(Self { http, key })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str, extra: &str) -> Result<T> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = format!("{BASE}{path}{sep}api_key={}{extra}", self.key);
        let res = self
            .http
            .get(&url)
            .send()
            .await
            // The URL inside a reqwest error carries the api_key; strip it so
            // an error line never publishes the key to whatever reads stdout.
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("requesting TMDB {path}"))?;
        let status = res.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("TMDB rejected the API key (401). Check $TMDB_API_KEY.");
        }
        if !status.is_success() {
            anyhow::bail!("TMDB {path} returned HTTP {status}");
        }
        res.json::<T>()
            .await
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("decoding TMDB {path}"))
    }

    /// Search movies and shows together, best matches first.
    ///
    /// `/search/multi` also returns people; they are dropped here rather than
    /// shown and rejected later, so every row in the picker is playable.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>> {
        let encoded = crate::torrentio::percent_encode(query);
        let res: SearchResponse = self
            .get(
                "/search/multi",
                &format!("&include_adult=false&query={encoded}"),
            )
            .await?;

        Ok(res
            .results
            .into_iter()
            .filter_map(|item| item.into_hit(None))
            .collect())
    }

    /// What people are watching now.
    ///
    /// `kind` of `None` is TMDB's `/trending/all`, which also returns people —
    /// dropped, as in search, because nothing here can play a person.
    pub async fn trending(&self, kind: Option<MediaType>, window: &str) -> Result<Vec<SearchHit>> {
        let path = match kind {
            None => "all",
            Some(MediaType::Movie) => "movie",
            Some(MediaType::Tv) => "tv",
        };
        let res: SearchResponse = self.get(&format!("/trending/{path}/{window}"), "").await?;
        Ok(res
            .results
            .into_iter()
            .filter_map(|item| item.into_hit(kind))
            .collect())
    }

    /// One page of the catalog for the current filters.
    ///
    /// `discover` results carry no `media_type` — the endpoint implies it — so
    /// it is filled in from the filters rather than read off each item.
    /// Titles without a poster are dropped, matching the site: on TMDB a
    /// missing poster reliably marks a stub entry with no usable metadata.
    pub async fn discover(&self, filters: &Filters, page: u32) -> Result<CatalogPage> {
        // TMDB's /discover/tv has no person filter — it accepts `with_cast`
        // and `with_people` and ignores both — so "everything this person is
        // in" has to be asked the other way round for series: their own TV
        // credits, filtered and paged here. Same question, same answer shape.
        if filters.kind == Kind::Tv && filters.cast_id > 0 {
            return self.person_tv_page(filters, page).await;
        }

        let media_type = match filters.kind {
            Kind::Movie => MediaType::Movie,
            Kind::Tv => MediaType::Tv,
        };
        let res: DiscoverResponse = self.get(&filters.discover_path(page), "").await?;

        let items = res
            .results
            .into_iter()
            .filter(|i| i.poster_path.is_some())
            .filter_map(|item| item.into_hit(Some(media_type)))
            .collect();

        Ok(CatalogPage {
            items,
            page,
            // TMDB refuses `page` above 500 whatever it reports as the total.
            total_pages: res.total_pages.unwrap_or(1).min(TMDB_MAX_PAGE),
        })
    }

    /// Everything about a movie.
    ///
    /// A missing IMDB id is reported as an empty one rather than as an error:
    /// it only stops *playback*, and the caller that just wants to show the
    /// title is entitled to the rest of the record.
    ///
    /// This is the lean version, and it is the one playback uses: resolving a
    /// release has no use for a cast list.
    pub async fn movie(&self, id: u32) -> Result<Movie> {
        let d: MovieDetail = self.get(&format!("/movie/{id}"), "").await?;
        Ok(d.into_movie(id))
    }

    /// The same movie plus everything a detail view shows, in one round trip.
    pub async fn movie_full(&self, id: u32) -> Result<(Movie, TitleDetail)> {
        let mut d: MovieDetail = self
            .get(
                &format!("/movie/{id}"),
                "&append_to_response=credits,videos,similar",
            )
            .await?;
        let detail = d.take_detail();
        Ok((d.into_movie(id), detail))
    }

    pub async fn show(&self, id: u32) -> Result<Show> {
        let d: ShowDetail = self
            .get(&format!("/tv/{id}"), "&append_to_response=external_ids")
            .await?;
        Self::into_show(d, id)
    }

    /// The same show plus everything a detail view shows, in one round trip.
    pub async fn show_full(&self, id: u32) -> Result<(Show, TitleDetail)> {
        let mut d: ShowDetail = self
            .get(
                &format!("/tv/{id}"),
                "&append_to_response=external_ids,credits,videos,similar",
            )
            .await?;
        let detail = d.take_detail();
        Ok((Self::into_show(d, id)?, detail))
    }

    /// Trailers and the rest, best first. Non-YouTube sites are dropped:
    /// nothing on this machine can play a Vimeo embed.
    pub async fn videos(&self, id: u32, kind: MediaType) -> Result<Vec<Video>> {
        let d: VideoResponse = self
            .get(&format!("/{}/{id}/videos", kind.key()), "")
            .await?;
        Ok(rank_videos(videos_of(Some(d))))
    }

    /// One page of everything a person is in, for series.
    ///
    /// The filters are applied here rather than by TMDB, which is the price of
    /// the endpoint not existing. Every field they need is on the credit —
    /// genre, original language, rating, first air date — so the answer is the
    /// same one `/discover/tv` would give if it could be asked. The one thing
    /// deliberately not carried over is the vote-count floor: a career is a
    /// curated list of a few hundred, not a catalog of a quarter million, and
    /// the floor exists to keep obscure entries off page one of the latter.
    async fn person_tv_page(&self, filters: &Filters, page: u32) -> Result<CatalogPage> {
        let d: SearchResponseWithCrew = self
            .get(&format!("/person/{}/tv_credits", filters.cast_id), "")
            .await?;
        let credits: Vec<Credit> = d
            .cast
            .into_iter()
            .chain(d.crew)
            .filter_map(|i| i.into_credit(Some(MediaType::Tv)))
            .collect();
        Ok(person_page(credits, filters, page))
    }

    /// A person and their filmography, in one round trip.
    ///
    /// `combined_credits` is the point: a cast click wants everything they
    /// were in, and asking for films and shows separately would be two calls
    /// and a merge for the same answer.
    pub async fn person(&self, id: u32) -> Result<PersonDetail> {
        let d: PersonResponse = self
            .get(
                &format!("/person/{id}"),
                "&append_to_response=combined_credits",
            )
            .await?;

        let combined = d.combined_credits.unwrap_or_default();
        let mut credits: Vec<Credit> = combined
            .cast
            .into_iter()
            .filter_map(|i| i.into_credit(None))
            .collect();
        // Crew credits too, filtered to the jobs a viewer recognises —
        // otherwise a director's page is empty, and a "Thanks" credit on a
        // blockbuster would outrank the films they actually made.
        credits.extend(
            combined
                .crew
                .into_iter()
                .filter_map(|i| i.into_credit(None))
                .filter_map(|mut c| {
                    let (_, label) = job_rank(&c.job)?;
                    c.job = label.to_string();
                    Some(c)
                }),
        );

        Ok(PersonDetail {
            id: d.id,
            name: d.name.unwrap_or_else(|| format!("Person {id}")),
            profile: d.profile_path.unwrap_or_default(),
            biography: d.biography.unwrap_or_default(),
            known_for: d.known_for_department.unwrap_or_default(),
            birthday: d.birthday.filter(|s| !s.is_empty()),
            deathday: d.deathday.filter(|s| !s.is_empty()),
            place_of_birth: d.place_of_birth.unwrap_or_default(),
            credits: rank_credits(credits),
        })
    }

    fn into_show(d: ShowDetail, id: u32) -> Result<Show> {
        let seasons = d
            .seasons
            .unwrap_or_default()
            .into_iter()
            // Season 0 is specials; it rarely has usable torrents and it is
            // never what someone means by "next episode".
            .filter(|s| s.season_number > 0 && s.episode_count.unwrap_or(0) > 0)
            .map(|s| Season {
                number: s.season_number,
                episode_count: s.episode_count.unwrap_or(0),
                name: s
                    .name
                    .unwrap_or_else(|| format!("Season {}", s.season_number)),
                poster: s.poster_path.unwrap_or_default(),
                year: year_of(&s.air_date),
            })
            .collect::<Vec<_>>();

        anyhow::ensure!(!seasons.is_empty(), "TMDB lists no seasons for this show");

        let default_runtime = d
            .episode_run_time
            .and_then(|v| v.into_iter().find(|r| *r > 0))
            .unwrap_or(FALLBACK_EPISODE_MINUTES);

        Ok(Show {
            tmdb_id: id,
            name: d.name.unwrap_or_else(|| format!("Show {id}")),
            original_name: d.original_name.unwrap_or_default(),
            year: year_of(&d.first_air_date),
            imdb_id: d
                .external_ids
                .and_then(|e| e.imdb_id)
                .filter(|s| s.starts_with("tt"))
                .unwrap_or_default(),
            seasons,
            default_runtime_secs: default_runtime * 60,
            vote: d.vote_average.unwrap_or(0.0),
            poster: d.poster_path.unwrap_or_default(),
            backdrop: d.backdrop_path.unwrap_or_default(),
            overview: d.overview.unwrap_or_default(),
            genres: genre_names(d.genres),
        })
    }

    /// One season's episodes. Takes the show's id and typical runtime rather
    /// than a fetched `Show`, so callers that already know the season can run
    /// this request *concurrently* with the show fetch instead of after it —
    /// on a panel click that is the difference between one round trip and two.
    pub async fn episodes(
        &self,
        show_id: u32,
        season: u32,
        default_runtime_secs: u32,
    ) -> Result<Vec<Episode>> {
        let d: SeasonDetail = self
            .get(&format!("/tv/{show_id}/season/{season}"), "")
            .await?;
        Ok(d.episodes
            .unwrap_or_default()
            .into_iter()
            .map(|e| Episode {
                season,
                number: e.episode_number,
                name: e
                    .name
                    .unwrap_or_else(|| format!("Episode {}", e.episode_number)),
                runtime_secs: e
                    .runtime
                    .filter(|r| *r > 0)
                    .map(|r| r * 60)
                    .unwrap_or(default_runtime_secs),
                overview: e.overview.unwrap_or_default(),
                still: e.still_path.unwrap_or_default(),
                air_date: e.air_date.unwrap_or_default(),
                vote: e.vote_average.unwrap_or(0.0),
            })
            .collect())
    }
}

impl MovieDetail {
    fn into_movie(self, id: u32) -> Movie {
        Movie {
            id,
            title: self.title.unwrap_or_else(|| format!("Movie {id}")),
            original_title: self.original_title.unwrap_or_default(),
            year: year_of(&self.release_date),
            imdb_id: self
                .imdb_id
                .filter(|s| s.starts_with("tt"))
                .unwrap_or_default(),
            runtime_secs: self
                .runtime
                .filter(|r| *r > 0)
                .unwrap_or(FALLBACK_MOVIE_MINUTES)
                * 60,
            vote: self.vote_average.unwrap_or(0.0),
            poster: self.poster_path.unwrap_or_default(),
            backdrop: self.backdrop_path.unwrap_or_default(),
            overview: self.overview.unwrap_or_default(),
            genres: genre_names(self.genres),
        }
    }

    /// Lift out the appended half, leaving the base record intact for
    /// `into_movie`. Everything taken here is `None` on the lean fetch, so the
    /// result is simply empty rather than wrong.
    fn take_detail(&mut self) -> TitleDetail {
        TitleDetail {
            tagline: self.tagline.take().unwrap_or_default(),
            status: self.status.take().unwrap_or_default(),
            homepage: self.homepage.take().unwrap_or_default(),
            cast: cast_of(self.credits.as_mut()),
            crew: crew_of(self.credits.take(), &[]),
            videos: rank_videos(videos_of(self.videos.take())),
            studios: named(self.production_companies.take()),
            countries: country_codes(self.production_countries.take()),
            languages: language_names(self.spoken_languages.take()),
            similar: similar_hits(self.similar.take(), MediaType::Movie),
            movie: Some(MovieFacts {
                budget: self.budget.unwrap_or(0),
                revenue: self.revenue.unwrap_or(0),
                release_date: self.release_date.clone().unwrap_or_default(),
            }),
            show: None,
        }
    }
}

impl ShowDetail {
    fn take_detail(&mut self) -> TitleDetail {
        // A show's creators are not in its crew list — TMDB keeps them in a
        // field of their own — so they are folded in here as "Creator", which
        // is the credit a viewer expects to see on a series.
        let creators: Vec<CrewMember> = self
            .created_by
            .take()
            .unwrap_or_default()
            .into_iter()
            .map(|c| CrewMember {
                id: c.id,
                name: c.name.unwrap_or_default(),
                job: "Creator".into(),
                profile: c.profile_path.unwrap_or_default(),
            })
            .collect();

        TitleDetail {
            tagline: self.tagline.take().unwrap_or_default(),
            status: self.status.take().unwrap_or_default(),
            homepage: self.homepage.take().unwrap_or_default(),
            cast: cast_of(self.credits.as_mut()),
            crew: crew_of(self.credits.take(), &creators),
            videos: rank_videos(videos_of(self.videos.take())),
            studios: named(self.production_companies.take()),
            // A show carries its own origin country; `production_countries` is
            // often empty on series, so it is only the fallback.
            countries: match self.origin_country.take() {
                Some(list) if !list.is_empty() => list,
                _ => country_codes(self.production_countries.take()),
            },
            languages: language_names(self.spoken_languages.take()),
            similar: similar_hits(self.similar.take(), MediaType::Tv),
            movie: None,
            show: Some(ShowFacts {
                season_count: self.number_of_seasons.unwrap_or(0),
                episode_count: self.number_of_episodes.unwrap_or(0),
                first_air: self.first_air_date.clone().unwrap_or_default(),
                last_air: self.last_air_date.clone().unwrap_or_default(),
                networks: named(self.networks.take()),
                in_production: self.in_production.unwrap_or(false),
            }),
        }
    }
}

fn genre_names(genres: Option<Vec<GenreRef>>) -> Vec<String> {
    genres
        .unwrap_or_default()
        .into_iter()
        .filter_map(|g| g.name)
        .collect()
}

fn named(list: Option<Vec<NamedRef>>) -> Vec<String> {
    list.unwrap_or_default()
        .into_iter()
        .filter_map(|n| n.name)
        .filter(|n| !n.is_empty())
        .collect()
}

fn country_codes(list: Option<Vec<CountryRef>>) -> Vec<String> {
    list.unwrap_or_default()
        .into_iter()
        .filter_map(|c| c.iso_3166_1)
        .collect()
}

/// English names, because the panel's audience reads "Hindi", not "हिन्दी".
fn language_names(list: Option<Vec<LanguageRef>>) -> Vec<String> {
    list.unwrap_or_default()
        .into_iter()
        .filter_map(|l| l.english_name.filter(|s| !s.is_empty()).or(l.name))
        .filter(|n| !n.is_empty())
        .collect()
}

fn similar_hits(list: Option<SearchResponse>, implied: MediaType) -> Vec<SearchHit> {
    list.unwrap_or_default()
        .results
        .into_iter()
        // The stub rule `discover` already applies: on TMDB a missing poster
        // reliably marks an entry with no usable metadata, and a shelf cannot
        // draw it anyway.
        .filter(|i| i.poster_path.is_some())
        .filter_map(|i| i.into_hit(Some(implied)))
        .take(MAX_SIMILAR)
        .collect()
}

/// Billing order, capped. TMDB sends `order` on every entry, but not sorted.
fn cast_of(credits: Option<&mut CreditsResponse>) -> Vec<CastMember> {
    let Some(credits) = credits else {
        return Vec::new();
    };
    let mut list: Vec<CastMember> = std::mem::take(&mut credits.cast)
        .into_iter()
        .map(|c| CastMember {
            id: c.id,
            name: c.name.unwrap_or_default(),
            character: c.character.unwrap_or_default(),
            profile: c.profile_path.unwrap_or_default(),
            order: c.order.unwrap_or(u32::MAX),
        })
        .filter(|c| !c.name.is_empty())
        .collect();
    list.sort_by_key(|c| c.order);
    list.truncate(MAX_CAST);
    list
}

fn crew_of(credits: Option<CreditsResponse>, extra: &[CrewMember]) -> Vec<CrewMember> {
    let mut all: Vec<CrewMember> = extra.to_vec();
    all.extend(
        credits
            .unwrap_or_default()
            .crew
            .into_iter()
            .map(|c| CrewMember {
                id: c.id,
                name: c.name.unwrap_or_default(),
                job: c.job.unwrap_or_default(),
                profile: c.profile_path.unwrap_or_default(),
            })
            .filter(|c| !c.name.is_empty()),
    );
    notable_crew(all)
}

fn videos_of(list: Option<VideoResponse>) -> Vec<Video> {
    list.unwrap_or_default()
        .results
        .into_iter()
        .map(|v| Video {
            key: v.key.unwrap_or_default(),
            site: v.site.unwrap_or_default(),
            kind: v.kind.unwrap_or_default(),
            name: v.name.unwrap_or_default(),
            official: v.official.unwrap_or(false),
            published: v.published_at.unwrap_or_default(),
        })
        .collect()
}

/// The crew jobs a viewer cares about, best first.
///
/// The order doubles as the tie-break when TMDB files one person under several
/// jobs — which it does constantly — so a writer-director reads as the
/// director rather than as whichever row happened to come back first.
const NOTABLE_JOBS: &[&str] = &[
    "Director",
    "Creator",
    "Writer",
    "Screenplay",
    "Executive Producer",
    "Composer",
];

/// Where a TMDB job sits in `NOTABLE_JOBS`, and what to call it.
///
/// The label is not always TMDB's own word: their crew list has no "Composer",
/// it has "Original Music Composer", which is not what anyone would put on a
/// poster.
fn job_rank(job: &str) -> Option<(usize, &'static str)> {
    let label = match job {
        "Director" => "Director",
        "Creator" | "Series Creator" => "Creator",
        "Writer" => "Writer",
        "Screenplay" => "Screenplay",
        "Executive Producer" => "Executive Producer",
        "Composer" | "Original Music Composer" => "Composer",
        _ => return None,
    };
    NOTABLE_JOBS
        .iter()
        .position(|j| *j == label)
        .map(|i| (i, label))
}

/// Keep the recognisable jobs, one row per person, best job first.
fn notable_crew(crew: Vec<CrewMember>) -> Vec<CrewMember> {
    let mut ranked: Vec<(usize, CrewMember)> = crew
        .into_iter()
        .filter_map(|mut m| {
            let (rank, label) = job_rank(&m.job)?;
            m.job = label.to_string();
            Some((rank, m))
        })
        .collect();
    // Stable, so TMDB's own ordering still breaks ties inside one job.
    ranked.sort_by_key(|(rank, _)| *rank);

    let mut seen: HashSet<u32> = HashSet::new();
    ranked
        .into_iter()
        .filter(|(_, m)| seen.insert(m.id))
        .map(|(_, m)| m)
        .take(MAX_CREW)
        .collect()
}

/// How interesting a video kind is. Anything unrecognised sorts last rather
/// than being dropped — TMDB adds types faster than this list can track them.
fn video_rank(kind: &str) -> usize {
    match kind {
        "Trailer" => 0,
        "Teaser" => 1,
        "Clip" => 2,
        "Featurette" => 3,
        "Behind the Scenes" => 4,
        _ => 5,
    }
}

/// Best first: official trailers, then teasers, then everything else, newest
/// inside each kind. Non-YouTube entries go entirely — nothing here can play
/// them, and offering a trailer that cannot open is worse than offering none.
fn rank_videos(videos: Vec<Video>) -> Vec<Video> {
    let mut list: Vec<Video> = videos
        .into_iter()
        .filter(|v| !v.key.is_empty() && v.site.eq_ignore_ascii_case("YouTube"))
        .collect();
    list.sort_by(|a, b| {
        video_rank(&a.kind)
            .cmp(&video_rank(&b.kind))
            .then(b.official.cmp(&a.official))
            // ISO-8601 sorts chronologically as text, so newest is just the
            // reversed string comparison.
            .then(b.published.cmp(&a.published))
    });
    list
}

/// TMDB TV genres for formats nobody browses a filmography to find: Talk,
/// News, Reality.
const CHAT_SHOW_GENRES: &[u32] = &[10767, 10763, 10764];

/// Whether a credit is someone turning up as themselves rather than playing
/// anyone.
///
/// This is not tidiness. TMDB's `popularity` is dominated by shows that air
/// every weeknight, so sorting a career by it puts *The Tonight Show* and
/// *Watch What Happens Live* above Breaking Bad on Bryan Cranston's page —
/// measured, not hypothetical. Both signals TMDB gives are used: the format of
/// the show, and the fact that the "character" is the person.
fn is_appearance(character: &str, genre_ids: &[u32]) -> bool {
    if genre_ids.iter().any(|g| CHAT_SHOW_GENRES.contains(g)) {
        return true;
    }
    let c = character.trim().to_ascii_lowercase();
    // "Self", "Self - Guest", "Himself (archive footage)" — always the word
    // first, so a role literally named "Herself" in a drama is safe from this.
    ["self", "himself", "herself", "themselves"]
        .iter()
        .any(|word| c == *word || c.starts_with(&format!("{word} ")))
}

/// Rows per page when a listing is paged here rather than by TMDB. Matches
/// what `/discover` returns, so a caller's paging maths does not have to
/// branch on which endpoint answered.
const PERSON_PAGE_SIZE: usize = 20;

/// One page of a person's TV credits, filtered and sorted the way `discover`
/// would have.
fn person_page(credits: Vec<Credit>, filters: &Filters, page: u32) -> CatalogPage {
    let in_years = |year: &str| match filters.years {
        None => true,
        Some((from, to)) => year
            .parse::<u32>()
            .map(|y| (from..=to).contains(&y))
            .unwrap_or(false),
    };

    let mut list: Vec<Credit> = credits
        .into_iter()
        .filter(|c| !c.hit.poster.is_empty())
        .filter(|c| !c.appearance)
        .filter(|c| filters.genre_id == 0 || c.genre_ids.contains(&filters.genre_id))
        .filter(|c| filters.language.is_empty() || c.language == filters.language)
        .filter(|c| filters.min_rating == 0 || c.hit.vote >= f64::from(filters.min_rating))
        .filter(|c| in_years(&c.hit.year))
        .collect();

    list.sort_by(|a, b| match filters.sort {
        // Box office has no TV equivalent, here as in `discover_path`.
        Sort::Popular | Sort::BoxOffice => b
            .popularity
            .partial_cmp(&a.popularity)
            .unwrap_or(Ordering::Equal),
        Sort::TopRated => b.hit.vote.partial_cmp(&a.hit.vote).unwrap_or(Ordering::Equal),
        Sort::Newest => b.hit.year.cmp(&a.hit.year),
        Sort::Oldest => a.hit.year.cmp(&b.hit.year),
    });

    // A long-running show is one row however many parts they played in it.
    let mut seen: HashSet<u32> = HashSet::new();
    let items: Vec<SearchHit> = list
        .into_iter()
        .filter(|c| seen.insert(c.hit.id))
        .map(|c| c.hit)
        .collect();

    let total_pages = items.len().div_ceil(PERSON_PAGE_SIZE).max(1) as u32;
    let page = page.max(1);
    let start = (page as usize - 1) * PERSON_PAGE_SIZE;
    CatalogPage {
        items: items
            .into_iter()
            .skip(start)
            .take(PERSON_PAGE_SIZE)
            .collect(),
        page,
        total_pages,
    }
}

/// A filmography: famous work first, one row per title, capped.
fn rank_credits(credits: Vec<Credit>) -> Vec<Credit> {
    let mut list: Vec<Credit> = credits
        .into_iter()
        // Same stub rule as `discover` — and a poster is the whole row here.
        .filter(|c| !c.hit.poster.is_empty())
        .filter(|c| !c.appearance)
        .collect();
    list.sort_by(|a, b| {
        b.popularity
            .partial_cmp(&a.popularity)
            .unwrap_or(Ordering::Equal)
    });

    // One row per title: an actor credited as two characters in the same show,
    // or as both writer and director of a film, is still one thing to watch.
    // The first survivor wins, which after the sort is the richer entry.
    let mut seen: HashSet<(MediaType, u32)> = HashSet::new();
    list.into_iter()
        .filter(|c| seen.insert((c.hit.media_type, c.hit.id)))
        .take(MAX_CREDITS)
        .collect()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_of_takes_the_leading_four_digits() {
        assert_eq!(year_of(&Some("1999-10-15".into())), "1999");
        assert_eq!(year_of(&Some("".into())), "");
        assert_eq!(year_of(&None), "");
    }

    fn crew(id: u32, name: &str, job: &str) -> CrewMember {
        CrewMember {
            id,
            name: name.into(),
            job: job.into(),
            profile: String::new(),
        }
    }

    #[test]
    fn crew_keeps_only_jobs_a_viewer_recognises() {
        let kept = notable_crew(vec![
            crew(1, "Fincher", "Director"),
            crew(2, "A Runner", "Second Assistant Director"),
            crew(3, "Uhls", "Screenplay"),
            crew(4, "Someone", "Thanks"),
        ]);
        let jobs: Vec<&str> = kept.iter().map(|c| c.job.as_str()).collect();
        assert_eq!(jobs, ["Director", "Screenplay"]);
    }

    #[test]
    fn one_person_appears_once_under_their_best_job() {
        // TMDB files a writer-director under both; a card has room for one.
        let kept = notable_crew(vec![
            crew(7, "Gilligan", "Writer"),
            crew(7, "Gilligan", "Director"),
            crew(7, "Gilligan", "Executive Producer"),
        ]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].job, "Director");
    }

    #[test]
    fn the_score_is_credited_as_composer() {
        // TMDB has no "Composer" job on films, only this.
        let kept = notable_crew(vec![crew(9, "Dust Brothers", "Original Music Composer")]);
        assert_eq!(kept[0].job, "Composer");
    }

    #[test]
    fn crew_is_capped() {
        let many: Vec<CrewMember> = (0..40)
            .map(|i| crew(i, "Producer", "Executive Producer"))
            .collect();
        assert_eq!(notable_crew(many).len(), MAX_CREW);
    }

    fn video(kind: &str, official: bool, published: &str, key: &str) -> Video {
        Video {
            key: key.into(),
            site: "YouTube".into(),
            kind: kind.into(),
            name: key.into(),
            official,
            published: published.into(),
        }
    }

    #[test]
    fn videos_put_the_official_trailer_first() {
        let ranked = rank_videos(vec![
            video("Featurette", true, "2019-01-01", "feat"),
            video("Teaser", true, "2020-01-01", "teaser"),
            video("Trailer", false, "2021-01-01", "fanmade"),
            video("Trailer", true, "2019-09-24", "official"),
        ]);
        let keys: Vec<&str> = ranked.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(keys, ["official", "fanmade", "teaser", "feat"]);
    }

    #[test]
    fn newer_wins_inside_one_kind() {
        let ranked = rank_videos(vec![
            video("Trailer", true, "2010-01-01", "old"),
            video("Trailer", true, "2019-09-24", "new"),
        ]);
        assert_eq!(ranked[0].key, "new");
    }

    #[test]
    fn videos_we_cannot_play_are_dropped() {
        let mut vimeo = video("Trailer", true, "2021-01-01", "abc");
        vimeo.site = "Vimeo".into();
        let ranked = rank_videos(vec![vimeo, video("Clip", false, "2001-01-01", "yt")]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].key, "yt");
    }

    #[test]
    fn a_video_url_is_something_mpv_can_open() {
        assert_eq!(
            video("Trailer", true, "", "dfeUzm6KF4g").url(),
            "https://www.youtube.com/watch?v=dfeUzm6KF4g"
        );
    }

    fn credit(id: u32, kind: MediaType, popularity: f64, poster: &str) -> Credit {
        Credit {
            hit: SearchHit {
                id,
                media_type: kind,
                title: format!("Title {id}"),
                year: "2000".into(),
                vote: 7.0,
                poster: poster.into(),
                backdrop: String::new(),
                overview: String::new(),
            },
            character: String::new(),
            job: String::new(),
            popularity,
            appearance: false,
            genre_ids: Vec::new(),
            language: "en".into(),
        }
    }

    #[test]
    fn credits_lead_with_the_famous_work() {
        let ranked = rank_credits(vec![
            credit(1, MediaType::Movie, 3.0, "/a.jpg"),
            credit(2, MediaType::Tv, 90.0, "/b.jpg"),
            credit(3, MediaType::Movie, 20.0, "/c.jpg"),
        ]);
        let ids: Vec<u32> = ranked.iter().map(|c| c.hit.id).collect();
        assert_eq!(ids, [2, 3, 1]);
    }

    #[test]
    fn credits_drop_stubs_with_no_poster() {
        let ranked = rank_credits(vec![
            credit(1, MediaType::Movie, 99.0, ""),
            credit(2, MediaType::Movie, 1.0, "/b.jpg"),
        ]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].hit.id, 2);
    }

    #[test]
    fn a_title_appears_once_however_many_credits_it_gave() {
        let ranked = rank_credits(vec![
            credit(1, MediaType::Tv, 50.0, "/a.jpg"),
            credit(1, MediaType::Tv, 50.0, "/a.jpg"),
        ]);
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn a_film_and_a_show_sharing_an_id_are_different_titles() {
        // TMDB ids are per-kind, so 1396 is both a movie and Breaking Bad.
        let ranked = rank_credits(vec![
            credit(1396, MediaType::Tv, 50.0, "/a.jpg"),
            credit(1396, MediaType::Movie, 40.0, "/b.jpg"),
        ]);
        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn a_prolific_career_is_capped() {
        let many: Vec<Credit> = (0..200)
            .map(|i| credit(i, MediaType::Movie, i as f64, "/x.jpg"))
            .collect();
        assert_eq!(rank_credits(many).len(), MAX_CREDITS);
    }

    #[test]
    fn a_sofa_is_not_a_role() {
        // Both signals, and the exact strings TMDB uses.
        assert!(is_appearance("Self - Guest", &[35]));
        assert!(is_appearance("Self", &[]));
        assert!(is_appearance("Himself (archive footage)", &[]));
        // A talk show is one whatever the character says.
        assert!(is_appearance("Host", &[10767, 35]));
        assert!(is_appearance("", &[10764]));
    }

    #[test]
    fn a_part_that_merely_sounds_reflexive_is_still_a_part() {
        assert!(!is_appearance("Walter White", &[18, 80]));
        assert!(
            !is_appearance("Selfish Man", &[18]),
            "matched on the word, not the prefix"
        );
        assert!(!is_appearance("Myself", &[18]), "not one of TMDB's words");
        assert!(!is_appearance("", &[18, 80]), "a crew credit has no character");
    }

    #[test]
    fn guest_spots_do_not_outrank_the_work() {
        // Measured: TMDB rates a nightly talk show above Breaking Bad.
        let mut sofa = credit(59941, MediaType::Tv, 221.4, "/a.jpg");
        sofa.character = "Self - Guest".into();
        sofa.appearance = true;
        let mut role = credit(1396, MediaType::Tv, 147.5, "/b.jpg");
        role.character = "Walter White".into();
        let ranked = rank_credits(vec![sofa, role]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].hit.id, 1396);
    }

    /// A TV credit with a year and a genre, for the local `discover` path.
    fn tv_credit(id: u32, year: &str, vote: f64, popularity: f64, genres: &[u32]) -> Credit {
        let mut c = credit(id, MediaType::Tv, popularity, "/p.jpg");
        c.hit.year = year.into();
        c.hit.vote = vote;
        c.genre_ids = genres.to_vec();
        c
    }

    #[test]
    fn a_persons_tv_credits_honour_the_same_filters_as_the_catalog() {
        let credits = vec![
            tv_credit(1, "2008", 8.9, 100.0, &[18, 80]), // drama/crime
            tv_credit(2, "1995", 6.0, 200.0, &[35]),     // comedy, too old
            tv_credit(3, "2015", 8.4, 50.0, &[18]),      // drama
        ];
        let filters = Filters {
            genre_id: 18,
            years: Some((2000, 2020)),
            cast_id: 17419,
            ..Filters::new(Kind::Tv)
        };
        let page = person_page(credits, &filters, 1);
        let ids: Vec<u32> = page.items.iter().map(|h| h.id).collect();
        assert_eq!(ids, [1, 3], "genre and year both applied, popular first");
    }

    #[test]
    fn a_rating_filter_applies_locally_too() {
        let credits = vec![
            tv_credit(1, "2008", 8.9, 10.0, &[18]),
            tv_credit(2, "2009", 5.5, 99.0, &[18]),
        ];
        let filters = Filters {
            min_rating: 8,
            cast_id: 1,
            ..Filters::new(Kind::Tv)
        };
        let page = person_page(credits, &filters, 1);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, 1);
    }

    #[test]
    fn a_persons_tv_credits_respect_the_requested_sort() {
        let credits = vec![
            tv_credit(1, "2008", 7.0, 10.0, &[]),
            tv_credit(2, "1995", 9.0, 5.0, &[]),
            tv_credit(3, "2020", 8.0, 99.0, &[]),
        ];
        let by = |sort: Sort| {
            let f = Filters {
                sort,
                cast_id: 1,
                ..Filters::new(Kind::Tv)
            };
            person_page(credits.clone(), &f, 1)
                .items
                .iter()
                .map(|h| h.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(by(Sort::Popular), [3, 1, 2]);
        assert_eq!(by(Sort::TopRated), [2, 3, 1]);
        assert_eq!(by(Sort::Newest), [3, 1, 2]);
        assert_eq!(by(Sort::Oldest), [2, 1, 3]);
    }

    #[test]
    fn a_person_page_pages_like_a_catalog_page() {
        let credits: Vec<Credit> = (0..45)
            .map(|i| tv_credit(i, "2010", 7.0, f64::from(45 - i), &[]))
            .collect();
        let filters = Filters {
            cast_id: 1,
            ..Filters::new(Kind::Tv)
        };
        let first = person_page(credits.clone(), &filters, 1);
        assert_eq!(first.items.len(), PERSON_PAGE_SIZE);
        assert_eq!(first.total_pages, 3);
        assert!(first.has_next());
        let last = person_page(credits.clone(), &filters, 3);
        assert_eq!(last.items.len(), 5);
        assert!(!last.has_next());
        // Page 1 and page 2 must not overlap.
        let second = person_page(credits, &filters, 2);
        assert_eq!(second.items[0].id, 20);
    }

    #[test]
    fn an_empty_person_page_still_reports_one_page() {
        let filters = Filters {
            cast_id: 1,
            ..Filters::new(Kind::Tv)
        };
        let page = person_page(Vec::new(), &filters, 1);
        assert!(page.items.is_empty());
        assert_eq!(page.total_pages, 1);
        assert!(!page.has_next());
    }

    #[test]
    fn cast_comes_back_in_billing_order_and_capped() {
        let mut credits = CreditsResponse {
            cast: (0..30)
                .rev()
                .map(|i| CastItem {
                    id: i,
                    name: Some(format!("Actor {i}")),
                    character: Some("Someone".into()),
                    profile_path: None,
                    order: Some(i),
                })
                .collect(),
            crew: Vec::new(),
        };
        let cast = cast_of(Some(&mut credits));
        assert_eq!(cast.len(), MAX_CAST);
        assert_eq!(cast[0].order, 0);
        assert_eq!(cast[1].order, 1);
        assert!(cast[0].profile.is_empty(), "no photo is an empty path");
    }
}
