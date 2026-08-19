//! Catalog browsing: sorts, filters, and the TMDB `discover` query they build.
//!
//! Deliberately a port of kojev's `listPath` / `tvListPath` rather than a fresh
//! design, so browsing here turns up the same titles the site would. That
//! includes the parts that look arbitrary and are not:
//!
//! - **Vote-count floors.** "Top rated" without one surfaces films with a 10.0
//!   from four voters, so it requires 300 votes; any rating filter requires 100.
//! - **A relaxed floor when a language is set.** Niche original languages have
//!   far fewer rated titles, and the usual floor empties the list — so it drops
//!   to 50.
//! - **An upper date bound** on popularity and newest, so unreleased titles do
//!   not fill the first page.

use std::time::{SystemTime, UNIX_EPOCH};

/// What to sort a catalog listing by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    Popular,
    TopRated,
    Newest,
    Oldest,
    /// Movies only — TV has no revenue figures, so it falls back to popularity.
    BoxOffice,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Sort::Popular => "Popular",
            Sort::TopRated => "Top Rated",
            Sort::Newest => "Newest",
            Sort::Oldest => "Oldest",
            Sort::BoxOffice => "Box Office",
        }
    }

    pub fn parse(s: &str) -> Option<Sort> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "popular" | "popularity" => Some(Sort::Popular),
            "top_rated" | "top" | "rating" => Some(Sort::TopRated),
            "newest" | "new" | "latest" => Some(Sort::Newest),
            "oldest" | "old" => Some(Sort::Oldest),
            "box_office" | "box" | "revenue" => Some(Sort::BoxOffice),
            _ => None,
        }
    }

    /// Sorts offered for a media kind. Box office is movie-only.
    pub fn all(kind: Kind) -> Vec<Sort> {
        let mut v = vec![Sort::Popular, Sort::TopRated, Sort::Newest, Sort::Oldest];
        if kind == Kind::Movie {
            v.push(Sort::BoxOffice);
        }
        v
    }
}

impl std::fmt::Display for Sort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Movie,
    Tv,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Movie => "Movies",
            Kind::Tv => "TV Series",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// TMDB's fixed movie-genre ids. Hardcoded, as on the site, to avoid an API
/// call whose only purpose is populating a menu.
pub const MOVIE_GENRES: &[(u32, &str)] = &[
    (28, "Action"),
    (12, "Adventure"),
    (16, "Animation"),
    (35, "Comedy"),
    (80, "Crime"),
    (99, "Documentary"),
    (18, "Drama"),
    (10751, "Family"),
    (14, "Fantasy"),
    (36, "History"),
    (27, "Horror"),
    (10402, "Music"),
    (9648, "Mystery"),
    (10749, "Romance"),
    (878, "Science Fiction"),
    (53, "Thriller"),
    (10752, "War"),
    (37, "Western"),
];

/// TMDB's TV-genre ids, which are a different set from the movie ones.
pub const TV_GENRES: &[(u32, &str)] = &[
    (10759, "Action & Adventure"),
    (16, "Animation"),
    (35, "Comedy"),
    (80, "Crime"),
    (99, "Documentary"),
    (18, "Drama"),
    (10751, "Family"),
    (10762, "Kids"),
    (9648, "Mystery"),
    (10763, "News"),
    (10764, "Reality"),
    (10765, "Sci-Fi & Fantasy"),
    (10766, "Soap"),
    (10767, "Talk"),
    (10768, "War & Politics"),
    (37, "Western"),
];

/// Original-content languages, ordered for this audience: South Asian first,
/// then East/South-East Asian, then the rest.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("hi", "Hindi"),
    ("bn", "Bangla"),
    ("ta", "Tamil"),
    ("te", "Telugu"),
    ("ml", "Malayalam"),
    ("kn", "Kannada"),
    ("pa", "Punjabi"),
    ("ur", "Urdu"),
    ("ko", "Korean"),
    ("ja", "Japanese"),
    ("zh", "Chinese"),
    ("th", "Thai"),
    ("tr", "Turkish"),
    ("es", "Spanish"),
    ("en", "English"),
];

pub fn genres_for(kind: Kind) -> &'static [(u32, &'static str)] {
    match kind {
        Kind::Movie => MOVIE_GENRES,
        Kind::Tv => TV_GENRES,
    }
}

/// Resolve a genre by name or id against the list for this kind.
/// Matching is case-insensitive and accepts a unique prefix, so `sci` works.
pub fn parse_genre(kind: Kind, value: &str) -> Option<u32> {
    let v = value.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    let list = genres_for(kind);
    if let Ok(id) = v.parse::<u32>() {
        return list.iter().find(|(gid, _)| *gid == id).map(|(gid, _)| *gid);
    }
    if let Some((id, _)) = list.iter().find(|(_, n)| n.to_ascii_lowercase() == v) {
        return Some(*id);
    }
    let mut hits = list
        .iter()
        .filter(|(_, n)| n.to_ascii_lowercase().starts_with(&v));
    let first = hits.next()?;
    // Ambiguous prefix: refuse rather than silently pick one.
    if hits.next().is_some() {
        return None;
    }
    Some(first.0)
}

/// Resolve a language by ISO code or English name.
pub fn parse_language(value: &str) -> Option<&'static str> {
    let v = value.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    LANGUAGES
        .iter()
        .find(|(code, name)| *code == v || name.to_ascii_lowercase() == v)
        .map(|(code, _)| *code)
}

pub fn genre_name(kind: Kind, id: u32) -> Option<&'static str> {
    genres_for(kind)
        .iter()
        .find(|(gid, _)| *gid == id)
        .map(|(_, n)| *n)
}

pub fn language_name(code: &str) -> Option<&'static str> {
    LANGUAGES.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

/// The current browse state: what to list and how.
#[derive(Clone, Debug)]
pub struct Filters {
    pub kind: Kind,
    pub sort: Sort,
    /// 0 means any.
    pub min_rating: u32,
    /// 0 means all genres.
    pub genre_id: u32,
    /// Empty means all languages.
    pub language: String,
}

impl Filters {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            sort: Sort::Popular,
            min_rating: 0,
            genre_id: 0,
            language: String::new(),
        }
    }

    /// One-line description of the active filters, for the browser header.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.sort.label().to_string()];
        if self.genre_id > 0 {
            if let Some(n) = genre_name(self.kind, self.genre_id) {
                parts.push(n.to_string());
            }
        }
        if !self.language.is_empty() {
            if let Some(n) = language_name(&self.language) {
                parts.push(n.to_string());
            }
        }
        if self.min_rating > 0 {
            parts.push(format!("{}+", self.min_rating));
        }
        parts.join(" · ")
    }

    /// Build the TMDB `discover` path for a page.
    ///
    /// Mirrors kojev's builders exactly, including the vote-count floors — see
    /// the module docs for why each one is there.
    pub fn discover_path(&self, page: u32) -> String {
        let mut parts = vec![format!("page={page}")];
        if self.genre_id > 0 {
            parts.push(format!("with_genres={}", self.genre_id));
        }
        if !self.language.is_empty() {
            parts.push(format!("with_original_language={}", self.language));
        }

        let today = today();
        let mut vote_count_gte: u32 = 0;
        let (date_field, oldest_floor) = match self.kind {
            Kind::Movie => ("primary_release_date", "1920-01-01"),
            Kind::Tv => ("first_air_date", "1940-01-01"),
        };

        match self.sort {
            Sort::TopRated => {
                parts.push("sort_by=vote_average.desc".into());
                vote_count_gte = 300;
            }
            Sort::Newest => {
                parts.push(format!("sort_by={date_field}.desc"));
                parts.push(format!("{date_field}.lte={today}"));
            }
            Sort::Oldest => {
                parts.push(format!("sort_by={date_field}.asc"));
                parts.push(format!("{date_field}.gte={oldest_floor}"));
            }
            // No TV equivalent of revenue, so TV falls through to popularity.
            Sort::BoxOffice if self.kind == Kind::Movie => {
                parts.push("sort_by=revenue.desc".into());
            }
            _ => {
                parts.push("sort_by=popularity.desc".into());
                parts.push(format!("{date_field}.lte={today}"));
            }
        }

        if self.min_rating > 0 {
            parts.push(format!("vote_average.gte={}", self.min_rating));
            vote_count_gte = vote_count_gte.max(100);
        }
        if !self.language.is_empty() {
            vote_count_gte = vote_count_gte.min(50);
        }
        if vote_count_gte > 0 {
            parts.push(format!("vote_count.gte={vote_count_gte}"));
        }

        let endpoint = match self.kind {
            Kind::Movie => "/discover/movie",
            Kind::Tv => "/discover/tv",
        };
        format!("{endpoint}?{}", parts.join("&"))
    }
}

/// Today as `YYYY-MM-DD`, UTC.
///
/// Hand-rolled from the civil-calendar algorithm rather than pulling in a date
/// crate for one function. UTC is fine here: the value only bounds a "not in
/// the future" filter, so being a few hours off at a day boundary changes
/// nothing a viewer would notice.
fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since the Unix epoch → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_calendar_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // 2024 was a leap year; day 60 of it is the 29th of February.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn today_is_a_well_formed_date() {
        let t = today();
        assert_eq!(t.len(), 10, "got {t}");
        assert_eq!(t.matches('-').count(), 2);
        assert!(t.starts_with("20"), "got {t}");
    }

    #[test]
    fn top_rated_requires_a_vote_floor() {
        // Without it, a 10.0 from four voters tops the list.
        let f = Filters {
            sort: Sort::TopRated,
            ..Filters::new(Kind::Movie)
        };
        assert!(f.discover_path(1).contains("vote_count.gte=300"));
        assert!(f.discover_path(1).contains("sort_by=vote_average.desc"));
    }

    #[test]
    fn a_rating_filter_adds_its_own_vote_floor() {
        let f = Filters {
            min_rating: 7,
            ..Filters::new(Kind::Movie)
        };
        let p = f.discover_path(1);
        assert!(p.contains("vote_average.gte=7"));
        assert!(p.contains("vote_count.gte=100"));
    }

    #[test]
    fn a_language_filter_relaxes_the_vote_floor() {
        // Bangla has far fewer heavily-voted titles; the 300 floor would empty
        // the list entirely.
        let f = Filters {
            sort: Sort::TopRated,
            language: "bn".into(),
            ..Filters::new(Kind::Movie)
        };
        let p = f.discover_path(1);
        assert!(p.contains("with_original_language=bn"));
        assert!(p.contains("vote_count.gte=50"), "got {p}");
    }

    #[test]
    fn popularity_excludes_unreleased_titles() {
        let f = Filters::new(Kind::Movie);
        assert!(f.discover_path(1).contains("primary_release_date.lte="));
    }

    #[test]
    fn tv_uses_first_air_date_not_release_date() {
        let f = Filters {
            sort: Sort::Newest,
            ..Filters::new(Kind::Tv)
        };
        let p = f.discover_path(1);
        assert!(p.starts_with("/discover/tv?"));
        assert!(p.contains("sort_by=first_air_date.desc"));
        assert!(!p.contains("primary_release_date"));
    }

    #[test]
    fn box_office_falls_back_to_popularity_for_tv() {
        let f = Filters {
            sort: Sort::BoxOffice,
            ..Filters::new(Kind::Tv)
        };
        let p = f.discover_path(1);
        assert!(p.contains("sort_by=popularity.desc"), "got {p}");
        assert!(!p.contains("revenue"));
    }

    #[test]
    fn box_office_is_not_offered_for_tv() {
        assert!(!Sort::all(Kind::Tv).contains(&Sort::BoxOffice));
        assert!(Sort::all(Kind::Movie).contains(&Sort::BoxOffice));
    }

    #[test]
    fn oldest_has_a_floor_so_undated_entries_do_not_win() {
        let f = Filters {
            sort: Sort::Oldest,
            ..Filters::new(Kind::Movie)
        };
        assert!(f
            .discover_path(1)
            .contains("primary_release_date.gte=1920-01-01"));
    }

    #[test]
    fn genres_resolve_by_name_id_and_unique_prefix() {
        assert_eq!(parse_genre(Kind::Movie, "Horror"), Some(27));
        assert_eq!(parse_genre(Kind::Movie, "horror"), Some(27));
        assert_eq!(parse_genre(Kind::Movie, "27"), Some(27));
        assert_eq!(parse_genre(Kind::Movie, "sci"), Some(878));
    }

    #[test]
    fn an_ambiguous_genre_prefix_is_refused_rather_than_guessed() {
        // "a" matches both Action and Adventure and Animation.
        assert_eq!(parse_genre(Kind::Movie, "a"), None);
    }

    #[test]
    fn genre_ids_are_kind_specific() {
        // 28 is Action for movies and means nothing for TV.
        assert_eq!(parse_genre(Kind::Movie, "28"), Some(28));
        assert_eq!(parse_genre(Kind::Tv, "28"), None);
        assert_eq!(parse_genre(Kind::Tv, "10759"), Some(10759));
    }

    #[test]
    fn languages_resolve_by_code_or_name() {
        assert_eq!(parse_language("bn"), Some("bn"));
        assert_eq!(parse_language("Bangla"), Some("bn"));
        assert_eq!(parse_language("klingon"), None);
    }

    #[test]
    fn sort_parsing_accepts_the_obvious_spellings() {
        assert_eq!(Sort::parse("top-rated"), Some(Sort::TopRated));
        assert_eq!(Sort::parse("top_rated"), Some(Sort::TopRated));
        assert_eq!(Sort::parse("NEWEST"), Some(Sort::Newest));
        assert_eq!(Sort::parse("nonsense"), None);
    }

    #[test]
    fn summary_lists_every_active_filter() {
        let f = Filters {
            kind: Kind::Movie,
            sort: Sort::TopRated,
            min_rating: 8,
            genre_id: 27,
            language: "ko".into(),
        };
        assert_eq!(f.summary(), "Top Rated · Horror · Korean · 8+");
    }

    #[test]
    fn summary_of_defaults_is_just_the_sort() {
        assert_eq!(Filters::new(Kind::Movie).summary(), "Popular");
    }
}
