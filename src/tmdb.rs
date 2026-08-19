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

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::browse::{Filters, Kind};

const BASE: &str = "https://api.themoviedb.org/3";

/// TMDB rejects `page` above this, regardless of the `total_pages` it reports.
const TMDB_MAX_PAGE: u32 = 500;

/// Assumed runtime when TMDB reports none. Deliberately on the long side: a
/// too-long guess understates the bitrate, which makes us *more* willing to try
/// a release, and the live throughput probe still has the final say.
const FALLBACK_MOVIE_MINUTES: u32 = 110;
const FALLBACK_EPISODE_MINUTES: u32 = 45;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

pub struct Tmdb {
    http: reqwest::Client,
    key: String,
}

#[derive(Deserialize)]
struct SearchResponse {
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
    runtime: Option<u32>,
    release_date: Option<String>,
    vote_average: Option<f64>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    overview: Option<String>,
    genres: Option<Vec<GenreRef>>,
}

#[derive(Deserialize)]
struct GenreRef {
    name: Option<String>,
}

#[derive(Deserialize)]
struct ExternalIds {
    imdb_id: Option<String>,
}

#[derive(Deserialize)]
struct ShowDetail {
    name: Option<String>,
    first_air_date: Option<String>,
    episode_run_time: Option<Vec<u32>>,
    seasons: Option<Vec<SeasonSummary>>,
    external_ids: Option<ExternalIds>,
    vote_average: Option<f64>,
    poster_path: Option<String>,
    backdrop_path: Option<String>,
    overview: Option<String>,
    genres: Option<Vec<GenreRef>>,
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
        let encoded = urlencode(query);
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
    pub async fn movie(&self, id: u32) -> Result<Movie> {
        let d: MovieDetail = self.get(&format!("/movie/{id}"), "").await?;
        Ok(Movie {
            id,
            title: d.title.unwrap_or_else(|| format!("Movie {id}")),
            year: year_of(&d.release_date),
            imdb_id: d
                .imdb_id
                .filter(|s| s.starts_with("tt"))
                .unwrap_or_default(),
            runtime_secs: d
                .runtime
                .filter(|r| *r > 0)
                .unwrap_or(FALLBACK_MOVIE_MINUTES)
                * 60,
            vote: d.vote_average.unwrap_or(0.0),
            poster: d.poster_path.unwrap_or_default(),
            backdrop: d.backdrop_path.unwrap_or_default(),
            overview: d.overview.unwrap_or_default(),
            genres: genre_names(d.genres),
        })
    }

    pub async fn show(&self, id: u32) -> Result<Show> {
        let d: ShowDetail = self
            .get(&format!("/tv/{id}"), "&append_to_response=external_ids")
            .await?;

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

    pub async fn episodes(&self, show: &Show, season: u32) -> Result<Vec<Episode>> {
        let d: SeasonDetail = self
            .get(&format!("/tv/{}/season/{season}", show.tmdb_id), "")
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
                    .unwrap_or(show.default_runtime_secs),
                overview: e.overview.unwrap_or_default(),
                still: e.still_path.unwrap_or_default(),
                air_date: e.air_date.unwrap_or_default(),
                vote: e.vote_average.unwrap_or(0.0),
            })
            .collect())
    }
}

fn genre_names(genres: Option<Vec<GenreRef>>) -> Vec<String> {
    genres
        .unwrap_or_default()
        .into_iter()
        .filter_map(|g| g.name)
        .collect()
}

/// Percent-encode a query string.
///
/// Hand-rolled to keep the dependency list at what was approved — the input is
/// a search phrase, so only the unreserved set needs to survive untouched.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_preserves_unreserved_and_escapes_the_rest() {
        assert_eq!(urlencode("fight club"), "fight%20club");
        assert_eq!(urlencode("Mr.Robot-2_x~"), "Mr.Robot-2_x~");
        assert_eq!(urlencode("a&b=c?d"), "a%26b%3Dc%3Fd");
    }

    #[test]
    fn urlencode_handles_multibyte() {
        // Percent-encoding operates on UTF-8 bytes, not chars.
        assert_eq!(urlencode("é"), "%C3%A9");
    }

    #[test]
    fn year_of_takes_the_leading_four_digits() {
        assert_eq!(year_of(&Some("1999-10-15".into())), "1999");
        assert_eq!(year_of(&Some("".into())), "");
        assert_eq!(year_of(&None), "");
    }
}
