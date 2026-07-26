//! Pure parsers: JSON string or HTML → domain models.
//! No network, no encryption. All functions take plain values and are
//! unit-testable in isolation.
//!
//! ## Schemas (confirmed from live browser captures, 2026-07-25)
//!
//! **Catalog** (`GET /api/catalog/search`, decrypted):
//! ```json
//! {"data":[{"id":"1326","title":"...","poster_url":"...","season_year":2013,...}],
//!  "meta":{"total":2797,"page":1,"per_page":32,"total_pages":88}}
//! ```
//!
//! **Anime detail** (inline `<script>` on `/anime/{id}/{slug}`):
//! ```json
//! {"anime":{"id":1326,"title":{"userPreferred":"...","romaji":"..."},"description":"...",
//!   "coverImage":{"large":"..."},"genres":[...],"status":"Ended",
//!   "episodesList":[{"id":95409,"number":1,"title":"..."},...]},...}
//! ```
//!
//! **Episode sources** (`POST /api/sources/anime`, decrypted):
//! ```json
//! [{"id":361656,"url":"https://vidmoly.biz/embed-...","host":"vidmoly",
//!   "language":"VOSTFR","episodeId":95409},...]
//! ```

use crate::errors::{provider_changed, Error, Result};
use crate::models::{AnimeDetails, AnimeId, AnimeSummary, Episode, EpisodeId, PlayableSource, Quality};
use serde_json::Value;

pub fn parse_catalog(json: &str) -> Result<Vec<AnimeSummary>> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| provider_changed("catalog", format!("not valid JSON: {e}")))?;
    let arr = v["data"]
        .as_array()
        .ok_or_else(|| provider_changed("catalog", "missing 'data' array"))?;
    Ok(arr
        .iter()
        .map(|item| AnimeSummary {
            id: AnimeId(item["id"].as_str().unwrap_or_default().to_string()),
            title: item["title"].as_str().unwrap_or_default().to_string(),
            poster_url: item["poster_url"].as_str().map(str::to_string),
            year: item["season_year"].as_u64().map(|y| y as u16),
        })
        .collect())
}

/// Extract the inline anime JSON from a Nakanime detail HTML page.
///
/// The JSON is embedded verbatim in a `<script>` tag (no `src`) as
/// `{"anime": {...}, "watchedEpisodes": [...], ...}`. A second `<script>`
/// tag on the same page carries JSON-LD schema markup; we skip it by
/// checking that the content starts with `{"anime":`.
pub fn extract_anime_json_from_html(html: &str) -> Result<String> {
    use scraper::{Html, Selector};
    let doc = Html::parse_document(html);
    let sel = Selector::parse("script:not([src])").expect("static selector is valid");
    for script in doc.select(&sel) {
        let text = script.text().collect::<String>();
        let trimmed = text.trim();
        if trimmed.starts_with(r#"{"anime":"#) {
            return Ok(trimmed.to_string());
        }
    }
    Err(provider_changed(
        "anime_detail",
        "anime JSON not found in page <script> tags — page layout may have changed",
    ))
}

/// Parse the inline anime JSON extracted by [`extract_anime_json_from_html`].
pub fn parse_anime_details(json: &str) -> Result<AnimeDetails> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| provider_changed("anime_detail", format!("not valid JSON: {e}")))?;
    let anime = &v["anime"];
    if anime.is_null() {
        return Err(provider_changed(
            "anime_detail",
            "missing 'anime' key in script JSON",
        ));
    }
    let title = anime["title"]["userPreferred"]
        .as_str()
        .or_else(|| anime["title"]["romaji"].as_str())
        .unwrap_or_default()
        .to_string();
    let mut episodes: Vec<Episode> = anime["episodesList"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|ep| Episode {
                    id: EpisodeId(ep["id"].to_string()),
                    number: ep["number"].to_string(),
                    title: ep["title"].as_str().map(str::to_string),
                    season_id: ep["seasonId"].as_u64().map(|v| v as u32),
                })
                .collect()
        })
        .unwrap_or_default();
    // Sort by season then by episode number so seasons are grouped correctly.
    episodes.sort_by_key(|e| {
        let s = e.season_id.unwrap_or(u32::MAX);
        let n = e.number.parse::<u64>().unwrap_or(u64::MAX);
        (s, n)
    });
    Ok(AnimeDetails {
        id: AnimeId(anime["id"].to_string()),
        title,
        description: anime["description"].as_str().map(str::to_string),
        poster_url: anime["coverImage"]["large"].as_str().map(str::to_string),
        genres: anime["genres"]
            .as_array()
            .map(|g| {
                g.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        status: anime["status"].as_str().map(str::to_string),
        episodes,
    })
}

/// Convenience: parse `AnimeDetails` directly from a Nakanime detail HTML page.
pub fn parse_anime_details_from_html(html: &str) -> Result<AnimeDetails> {
    let json = extract_anime_json_from_html(html)?;
    parse_anime_details(&json)
}

/// Parse the decrypted sources array from `POST /api/sources/anime`.
/// Each source carries an embed-page URL that mpv (with yt-dlp) can resolve.
pub fn parse_episode_sources(json: &str) -> Result<Vec<PlayableSource>> {
    let arr: Vec<Value> = serde_json::from_str(json)
        .map_err(|e| provider_changed("sources", format!("not valid JSON: {e}")))?;
    if arr.is_empty() {
        return Err(Error::Resolve(
            "no sources available for this episode".into(),
        ));
    }
    Ok(arr
        .into_iter()
        .filter_map(|src| {
            let url = src["url"].as_str()?.to_string();
            let host = src["host"].as_str().unwrap_or("?");
            let lang = src["language"].as_str().unwrap_or("?");
            Some(PlayableSource {
                url,
                quality: Quality::Unknown,
                label: Some(format!("{host} ({lang})")),
                http_headers: vec![("Referer".to_string(), "https://nakanime.tv/".to_string())],
                subtitles: vec![],
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- catalog ---

    #[test]
    fn parse_catalog_rejects_non_json() {
        match parse_catalog("<html>blocked</html>") {
            Err(Error::ProviderChanged { context, .. }) => assert_eq!(context, "catalog"),
            other => panic!("expected ProviderChanged, got {other:?}"),
        }
    }

    #[test]
    fn parse_catalog_empty_data() {
        let items = parse_catalog(r#"{"data":[],"meta":{}}"#).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn parse_catalog_maps_fields() {
        let json = r#"{"data":[{"id":"1326","title":"Shingeki no Kyojin","poster_url":"https://img.example.com/p.jpg","season_year":2013}],"meta":{}}"#;
        let items = parse_catalog(json).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.0, "1326");
        assert_eq!(items[0].title, "Shingeki no Kyojin");
        assert_eq!(items[0].poster_url.as_deref(), Some("https://img.example.com/p.jpg"));
        assert_eq!(items[0].year, Some(2013));
    }

    // --- anime details ---

    #[test]
    fn parse_anime_details_maps_fields() {
        let json = r#"{"anime":{"id":1326,"title":{"userPreferred":"L'Attaque des Titans","romaji":"Shingeki no Kyojin","english":"Attack on Titan","native":"進撃の巨人"},"description":"In a world...","coverImage":{"large":"https://img.example.com/cover.jpg"},"genres":["Action","Drama"],"status":"Ended","episodesList":[{"id":95409,"number":1,"title":"Episode 1"}]}}"#;
        let details = parse_anime_details(json).unwrap();
        assert_eq!(details.id.0, "1326");
        assert_eq!(details.title, "L'Attaque des Titans");
        assert_eq!(details.description.as_deref(), Some("In a world..."));
        assert_eq!(details.poster_url.as_deref(), Some("https://img.example.com/cover.jpg"));
        assert_eq!(details.genres, vec!["Action", "Drama"]);
        assert_eq!(details.status.as_deref(), Some("Ended"));
        assert_eq!(details.episodes.len(), 1);
        assert_eq!(details.episodes[0].id.0, "95409");
        assert_eq!(details.episodes[0].number, "1");
        assert_eq!(details.episodes[0].title.as_deref(), Some("Episode 1"));
    }

    #[test]
    fn parse_anime_details_sorts_episodes_by_season_then_number() {
        // Server returns episodes out of order across two seasons.
        let json = r#"{"anime":{"id":1,"title":{"userPreferred":"Test"},"episodesList":[
            {"id":200,"number":1,"seasonId":20},
            {"id":101,"number":2,"seasonId":10},
            {"id":100,"number":1,"seasonId":10}
        ]}}"#;
        let details = parse_anime_details(json).unwrap();
        let ids: Vec<&str> = details.episodes.iter().map(|e| e.id.0.as_str()).collect();
        // S1 (seasonId=10): ep1=100, ep2=101; S2 (seasonId=20): ep1=200
        assert_eq!(ids, vec!["100", "101", "200"]);
        assert_eq!(details.episodes[0].season_id, Some(10));
        assert_eq!(details.episodes[2].season_id, Some(20));
    }

    #[test]
    fn parse_anime_details_falls_back_to_romaji() {
        let json = r#"{"anime":{"id":1,"title":{"userPreferred":null,"romaji":"Naruto"},"episodesList":[]}}"#;
        let details = parse_anime_details(json).unwrap();
        assert_eq!(details.title, "Naruto");
    }

    #[test]
    fn extract_anime_json_finds_correct_script() {
        let html = r#"<html><body>
            <script type="application/ld+json">{"@context":"https://schema.org"}</script>
            <script>{"anime":{"id":1},"watchedEpisodes":[]}</script>
        </body></html>"#;
        let json = extract_anime_json_from_html(html).unwrap();
        assert!(json.starts_with(r#"{"anime":"#));
    }

    #[test]
    fn extract_anime_json_errors_on_missing() {
        let html = "<html><body><script>var x = 1;</script></body></html>";
        match extract_anime_json_from_html(html) {
            Err(Error::ProviderChanged { context, .. }) => assert_eq!(context, "anime_detail"),
            other => panic!("expected ProviderChanged, got {other:?}"),
        }
    }

    // --- episode sources ---

    #[test]
    fn parse_episode_sources_maps_fields() {
        let json = r#"[{"id":361656,"url":"https://vidmoly.biz/embed-3yw9j0gyz2a9.html","host":"vidmoly","language":"VOSTFR","episodeId":95409}]"#;
        let sources = parse_episode_sources(json).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].url, "https://vidmoly.biz/embed-3yw9j0gyz2a9.html");
        assert!(sources[0]
            .http_headers
            .iter()
            .any(|(k, _)| k == "Referer"));
    }

    #[test]
    fn parse_episode_sources_empty_is_error() {
        match parse_episode_sources("[]") {
            Err(Error::Resolve(_)) => {}
            other => panic!("expected Resolve error, got {other:?}"),
        }
    }

    #[test]
    fn parse_episode_sources_rejects_non_json() {
        match parse_episode_sources("not json") {
            Err(Error::ProviderChanged { context, .. }) => assert_eq!(context, "sources"),
            other => panic!("expected ProviderChanged, got {other:?}"),
        }
    }
}
