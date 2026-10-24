//! `dekho api` — the same catalog, shaped for a program instead of a person.
//!
//! Every verb answers with exactly one JSON object on stdout and nothing else,
//! including on failure, so a caller can parse stdout blind and never has to
//! read stderr to find out what happened. No engine is started and no torrent
//! is touched: this is a metadata surface, and the panel it exists for opens
//! and closes far more often than anything gets played. (`releases` also asks
//! the indexers — still plain HTTP, still no engine.)

use std::collections::HashMap;
use std::future::Future;

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use crate::apicache;
use crate::browse::{self, Filters, Kind};
use crate::history::History;
use crate::tmdb::{
    CastMember, Credit, CrewMember, Episode, MediaType, PersonDetail, SearchHit, TitleDetail, Tmdb,
    Video,
};

/// One search hit, as every list-shaped verb returns it.
fn hit(h: &SearchHit) -> Value {
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

/// A hit as Browse draws a row: the plain hit, plus the English name of its
/// original language and the names of its genres. `languages` is TMDB's
/// cached table; the compiled list answers when the table could not be had.
fn listed(h: &SearchHit, languages: &HashMap<String, String>) -> Value {
    let mut value = hit(h);
    let language = languages
        .get(&h.language)
        .map(String::as_str)
        .or_else(|| browse::language_name(&h.language))
        .unwrap_or_default();
    let kind = match h.media_type {
        MediaType::Movie => Kind::Movie,
        MediaType::Tv => Kind::Tv,
    };
    let genres: Vec<&str> = h
        .genre_ids
        .iter()
        .filter_map(|id| browse::genre_name(kind, *id))
        .collect();
    if let Some(map) = value.as_object_mut() {
        map.insert("language".into(), json!(language));
        map.insert("genres".into(), json!(genres));
    }
    value
}

/// The cached language table, empty when TMDB cannot give it.
async fn language_table(tmdb: &Tmdb) -> HashMap<String, String> {
    name_table("language-table", tmdb.languages()).await
}

fn items(list: Vec<Value>) -> Value {
    json!({ "items": list })
}

pub async fn search(tmdb: &Tmdb, query: &str) -> Result<Value> {
    let (hits, languages) = tokio::join!(tmdb.search(query), language_table(tmdb));
    Ok(items(hits?.iter().map(|h| listed(h, &languages)).collect()))
}

pub async fn trending(tmdb: &Tmdb, kind: Option<MediaType>, window: &str) -> Result<Value> {
    let (hits, languages) = tokio::join!(tmdb.trending(kind, window), language_table(tmdb));
    Ok(items(hits?.iter().map(|h| listed(h, &languages)).collect()))
}

/// People whose name matches, for a search box that wants a person.
pub async fn people(tmdb: &Tmdb, query: &str) -> Result<Value> {
    let found = tmdb.search_people(query).await?;
    Ok(items(
        found
            .iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "name": p.name,
                    "known_for_department": p.known_for_department,
                    "profile": p.profile,
                })
            })
            .collect(),
    ))
}

/// One page of the catalog, with the page numbers a caller needs to walk it.
pub async fn discover(tmdb: &Tmdb, filters: &Filters, page: u32) -> Result<Value> {
    let (catalog, languages) =
        tokio::join!(tmdb.discover(filters, page.max(1)), language_table(tmdb));
    let catalog = catalog?;
    Ok(json!({
        "items": catalog.items.iter().map(|h| listed(h, &languages)).collect::<Vec<_>>(),
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

/// A code-to-name table from TMDB's configuration, through the API cache.
/// Empty when the fetch fails and nothing is cached.
async fn name_table<F>(verb: &str, fetch: F) -> HashMap<String, String>
where
    F: Future<Output = Result<Vec<(String, String)>>>,
{
    let plan = (
        apicache::key(&[verb]),
        apicache::ttl_secs(verb).unwrap_or(0),
    );
    let value = apicache::serve(Some(&plan), false, async {
        let rows = fetch.await?;
        Ok(json!({"items": rows.iter().map(|(c, n)| json!([c, n])).collect::<Vec<_>>()}))
    })
    .await
    .unwrap_or_default();
    value["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some((row[0].as_str()?.to_string(), row[1].as_str()?.to_string())))
        .collect()
}

/// Everything about one title, seasons included.
///
/// `runtime` is the same figure the bitrate gate divides by, fallback and all,
/// so what a caller displays and what dekho reasons about cannot drift apart.
///
/// The cast, crew, videos and similar titles arrive in the *same* TMDB request
/// as the rest (`append_to_response`), because a detail view that opens five
/// round trips deep opens slowly on the link this is for.
pub async fn title(tmdb: &Tmdb, id: u32, kind: MediaType) -> Result<Value> {
    let (mut base, detail) = match kind {
        MediaType::Movie => {
            let (m, d) = tmdb.movie_full(id).await?;
            (
                json!({
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
                }),
                d,
            )
        }
        MediaType::Tv => {
            let (s, d) = tmdb.show_full(id).await?;
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
            (
                json!({
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
                }),
                d,
            )
        }
    };

    // The tables are cached for a month by `apicache`, so a title costs no
    // extra request once they are on disk. A table that cannot be had leaves
    // the names TMDB wrote in the record, and drops what has none.
    let (countries, languages) = tokio::join!(
        name_table("country-table", tmdb.countries()),
        name_table("language-table", tmdb.languages())
    );
    let place_names: Vec<String> = detail
        .countries
        .iter()
        .filter_map(|p| {
            countries
                .get(&p.code)
                .cloned()
                .or_else(|| Some(p.name.clone()).filter(|n| !n.is_empty()))
        })
        .collect();
    let language = languages
        .get(&detail.original_language)
        .cloned()
        .unwrap_or_default();

    if let Some(map) = base.as_object_mut() {
        map.insert("language".into(), json!(language));
        // Browse filters by the code, so f on a title page can choose this language.
        map.insert("language_code".into(), json!(detail.original_language));
        for (key, value) in detail_fields(&detail, place_names) {
            map.insert(key.to_string(), value);
        }
    }
    Ok(base)
}

/// The additive half of `title`. Every key here is new; nothing above it moves.
fn detail_fields(d: &TitleDetail, countries: Vec<String>) -> Vec<(&'static str, Value)> {
    let mut fields = vec![
        ("tagline", json!(d.tagline)),
        ("status", json!(d.status)),
        ("homepage", json!(d.homepage)),
        // The best one, already ranked. "" rather than null, so a caller can
        // test it the same way it tests `poster`.
        (
            "trailer",
            json!(d.videos.first().map(Video::url).unwrap_or_default()),
        ),
        ("cast", json!(d.cast.iter().map(cast).collect::<Vec<_>>())),
        ("crew", json!(d.crew.iter().map(crew).collect::<Vec<_>>())),
        ("studios", json!(d.studios)),
        ("countries", json!(countries)),
        ("languages", json!(d.languages)),
        ("similar", json!(d.similar.iter().map(hit).collect::<Vec<_>>())),
    ];
    if let Some(m) = &d.movie {
        fields.push(("budget", json!(m.budget)));
        fields.push(("revenue", json!(m.revenue)));
        fields.push(("release_date", json!(m.release_date)));
    }
    if let Some(s) = &d.show {
        fields.push(("season_count", json!(s.season_count)));
        fields.push(("episode_count", json!(s.episode_count)));
        fields.push(("first_air", json!(s.first_air)));
        fields.push(("last_air", json!(s.last_air)));
        fields.push(("networks", json!(s.networks)));
        fields.push(("in_production", json!(s.in_production)));
    }
    fields
}

fn cast(c: &CastMember) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "character": c.character,
        "profile": c.profile,
        "order": c.order,
    })
}

fn crew(c: &CrewMember) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "job": c.job,
        "profile": c.profile,
    })
}

/// Every trailer, teaser and clip TMDB knows about, best first.
pub async fn videos(tmdb: &Tmdb, id: u32, kind: MediaType) -> Result<Value> {
    let list = tmdb.videos(id, kind).await?;
    Ok(items(list.iter().map(video).collect()))
}

fn video(v: &Video) -> Value {
    json!({
        "key": v.key,
        "url": v.url(),
        "site": v.site,
        "type": v.kind,
        "name": v.name,
        "official": v.official,
        "published": v.published,
    })
}

/// One person and what they were in — what a cast click resolves to.
pub async fn person(tmdb: &Tmdb, id: u32) -> Result<Value> {
    let p: PersonDetail = tmdb.person(id).await?;
    Ok(json!({
        "id": p.id,
        "name": p.name,
        "profile": p.profile,
        "biography": p.biography,
        "known_for": p.known_for,
        // Dates are null rather than "" when TMDB has none: a missing deathday
        // is a living person, which is not the same fact as an unknown one.
        "birthday": p.birthday,
        "deathday": p.deathday,
        "place_of_birth": p.place_of_birth,
        "credits": {
            "directing": p.directing.iter().map(|c| credit(c, &c.job)).collect::<Vec<_>>(),
            "acting": p.acting.iter().map(|c| credit(c, &c.character)).collect::<Vec<_>>(),
            "writing": p.writing.iter().map(|c| credit(c, &c.job)).collect::<Vec<_>>(),
        },
    }))
}

/// An ordinary hit, plus the part the person had in it.
fn credit(c: &Credit, role: &str) -> Value {
    let mut value = hit(&c.hit);
    if let Some(map) = value.as_object_mut() {
        map.insert("role".into(), json!(role));
    }
    value
}

pub async fn episodes(tmdb: &Tmdb, id: u32, season: u32) -> Result<Value> {
    // Both fetched concurrently — the season list does not depend on the
    // show, only the runtime fallback below does, and serialising the two
    // used to double a panel's season-click latency.
    let (show, list) = tokio::join!(tmdb.show(id), tmdb.episodes(id, season, 0));
    let show = show?;
    let mut list = list?;
    for e in &mut list {
        // An episode with no runtime of its own inherits the show's typical
        // one, which is what sizes the buffer for it later.
        if e.runtime_secs == 0 {
            e.runtime_secs = show.default_runtime_secs;
        }
    }
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

/// Give each episode of an `episodes` answer its `watched` flag. Applied to
/// the answer as served, cached or not, since marks change more often than a
/// season does.
pub fn overlay_watched(value: &mut Value, id: u32, store: &History) {
    for e in value["items"].as_array_mut().into_iter().flatten() {
        let (Some(season), Some(episode)) = (e["season"].as_u64(), e["episode"].as_u64()) else {
            continue;
        };
        let watched = store.watched(id, season as u32, episode as u32);
        if let Some(map) = e.as_object_mut() {
            map.insert("watched".into(), json!(watched));
        }
    }
}

/// Mark or unmark one episode. Local state only, so a title never played can
/// be marked without a TMDB call.
pub fn watched(id: u32, season: u32, episode: u32, watched: bool) -> Result<Value> {
    let path = crate::history::path();
    let mut store = History::load(&path);
    store.mark(id, season, episode, watched, crate::history::now());
    store.save(&path)?;
    Ok(json!({ "watched": store.watched(id, season, episode) }))
}

/// Recently watched titles, newest first. Local state only — no TMDB call.
pub fn history(limit: usize) -> Result<Value> {
    let store = History::load(&crate::history::path());
    Ok(items(
        store.recent(limit).iter().map(|e| e.json()).collect(),
    ))
}

/// Download posters into the local cache and answer with their paths.
/// Every release for a title, in the order the interactive menu would show
/// them: wrong-title strangers dropped, best first, nothing else hidden. The
/// `hash` on each row is what `dekho play --release` takes back.
pub async fn releases(
    tmdb: &Tmdb,
    id: u32,
    kind: MediaType,
    season: Option<u32>,
    episode: Option<u32>,
) -> Result<Value> {
    let (imdb_id, titles) = match kind {
        MediaType::Movie => {
            let m = tmdb.movie(id).await?;
            (m.imdb_id, [m.title, m.original_title])
        }
        MediaType::Tv => {
            let s = tmdb.show(id).await?;
            (s.imdb_id, [s.name, s.original_name])
        }
    };
    anyhow::ensure!(
        !imdb_id.is_empty(),
        "TMDB has no IMDB id for this title, so no releases can be looked up"
    );
    let (season, episode) = match kind {
        MediaType::Tv => (
            Some(season.context("api releases for a series needs -s")?),
            Some(episode.context("api releases for a series needs -e")?),
        ),
        MediaType::Movie => (None, None),
    };

    let sources = crate::sources::Sources::new()?;
    let lookup = sources
        .candidates(&imdb_id, kind.torrentio(), season, episode)
        .await;
    let guard = crate::pick::title_guard(
        lookup.candidates,
        &[titles[0].as_str(), titles[1].as_str()],
    );
    let items: Vec<Value> = crate::pick::rank_for_menu(guard.kept)
        .iter()
        .map(|c| {
            json!({
                "hash": crate::sources::info_hash_of(&c.magnet),
                "title": c.title,
                "quality": c.quality.label(),
                "size_bytes": c.size_bytes,
                "size": c.size_bytes.map(crate::torrentio::format_bytes),
                "seeders": c.seeders,
                "audio": c.audio.label(),
            })
        })
        .collect();

    Ok(json!({
        "items": items,
        // Strangers filed under this IMDB id, dropped by the title guard —
        // carried so a panel can say why the list is shorter than the raw one.
        "dropped": guard.dropped,
        // Indexers that errored; their releases are simply absent.
        "failed": lookup.failed,
    }))
}

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
