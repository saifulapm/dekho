//! `dekho api` — the same catalog, shaped for a program instead of a person.
//!
//! Every verb answers with exactly one JSON object on stdout and nothing else,
//! including on failure, so a caller can parse stdout blind and never has to
//! read stderr to find out what happened. No engine is started and no torrent
//! is touched: this is a metadata surface, and the panel it exists for opens
//! and closes far more often than anything gets played.

use anyhow::Result;
use serde_json::{json, Map, Value};

use crate::browse::{self, Filters, Kind};
use crate::history::History;
use crate::tmdb::{Episode, MediaType, SearchHit, Tmdb};

/// One search hit, as every list-shaped verb returns it.
pub fn hit(h: &SearchHit) -> Value {
    json!({
        "id": h.id,
        "kind": h.media_type.key(),
        "title": h.title,
        "year": h.year,
        "vote": h.vote,
        "poster": h.poster,
        "backdrop": h.backdrop,
        "overview": h.overview,
    })
}

fn items(list: Vec<Value>) -> Value {
    json!({ "items": list })
}

pub async fn search(tmdb: &Tmdb, query: &str) -> Result<Value> {
    let hits = tmdb.search(query).await?;
    Ok(items(hits.iter().map(hit).collect()))
}

pub async fn trending(tmdb: &Tmdb, kind: Option<MediaType>, window: &str) -> Result<Value> {
    let hits = tmdb.trending(kind, window).await?;
    Ok(items(hits.iter().map(hit).collect()))
}

/// One page of the catalog, with the page numbers a caller needs to walk it.
pub async fn discover(tmdb: &Tmdb, filters: &Filters, page: u32) -> Result<Value> {
    let catalog = tmdb.discover(filters, page.max(1)).await?;
    Ok(json!({
        "items": catalog.items.iter().map(hit).collect::<Vec<_>>(),
        "page": catalog.page,
        "total_pages": catalog.total_pages,
    }))
}

pub fn genres(kind: Kind) -> Value {
    items(
        browse::genres_for(kind)
            .iter()
            .map(|(id, name)| json!({"id": id, "name": name}))
            .collect(),
    )
}

pub fn languages() -> Value {
    items(
        browse::LANGUAGES
            .iter()
            .map(|(code, name)| json!({"code": code, "name": name}))
            .collect(),
    )
}

/// Everything about one title, seasons included.
///
/// `runtime` is the same figure the bitrate gate divides by, fallback and all,
/// so what a caller displays and what dekho reasons about cannot drift apart.
pub async fn title(tmdb: &Tmdb, id: u32, kind: MediaType) -> Result<Value> {
    match kind {
        MediaType::Movie => {
            let m = tmdb.movie(id).await?;
            Ok(json!({
                "id": m.id,
                "kind": "movie",
                "title": m.title,
                "year": m.year,
                "vote": m.vote,
                "poster": m.poster,
                "backdrop": m.backdrop,
                "overview": m.overview,
                "genres": m.genres,
                "runtime": m.runtime_secs,
                "imdb_id": m.imdb_id,
                "seasons": Value::Array(Vec::new()),
            }))
        }
        MediaType::Tv => {
            let s = tmdb.show(id).await?;
            let seasons: Vec<Value> = s
                .seasons
                .iter()
                .map(|season| {
                    json!({
                        "number": season.number,
                        "episodes": season.episode_count,
                        "name": season.name,
                        "poster": season.poster,
                        "year": season.year,
                    })
                })
                .collect();
            Ok(json!({
                "id": s.tmdb_id,
                "kind": "tv",
                "title": s.name,
                "year": s.year,
                "vote": s.vote,
                "poster": s.poster,
                "backdrop": s.backdrop,
                "overview": s.overview,
                "genres": s.genres,
                "runtime": s.default_runtime_secs,
                "imdb_id": s.imdb_id,
                "seasons": seasons,
            }))
        }
    }
}

pub async fn episodes(tmdb: &Tmdb, id: u32, season: u32) -> Result<Value> {
    // The show first, because an episode with no runtime of its own inherits
    // the show's, and that is what sizes the buffer for it later.
    let show = tmdb.show(id).await?;
    let list = tmdb.episodes(&show, season).await?;
    Ok(items(list.iter().map(episode).collect()))
}

fn episode(e: &Episode) -> Value {
    json!({
        "season": e.season,
        "episode": e.number,
        "name": e.name,
        "overview": e.overview,
        "runtime": e.runtime_secs,
        "still": e.still,
        "air_date": e.air_date,
        "vote": e.vote,
    })
}

/// Recently watched titles, newest first. Local state only — no TMDB call.
pub fn history(limit: usize) -> Result<Value> {
    let store = History::load(&crate::history::path());
    Ok(items(
        store.recent(limit).iter().map(|e| e.json()).collect(),
    ))
}

/// Download posters into the local cache and answer with their paths.
pub async fn prefetch(size: &str, paths: &[String]) -> Result<Value> {
    crate::prefetch::run(size, paths).await
}

/// The failure shape. On stdout like everything else, so one parser handles
/// both outcomes.
pub fn error(message: &str) -> Value {
    let mut object = Map::new();
    object.insert("error".into(), json!(message));
    Value::Object(object)
}
