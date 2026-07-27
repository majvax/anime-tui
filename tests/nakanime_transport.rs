//! Integration tests for the Nakanime provider's HTTP transport and parsers.
//! All HTTP is mocked (wiremock) — never the live site.

use anime_tui::errors::Error;
use anime_tui::models::{AnimeId, EpisodeId};
use anime_tui::provider::nakanime::Nakanime;
use anime_tui::provider::Provider;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider_for(server: &MockServer) -> Nakanime {
    Nakanime::new(reqwest::Client::new(), server.uri())
}

/// Encrypt with an arithmetic XOR key: key[i] = (start + i*step) % 256.
/// dynamic_decrypt recovers (start,step) at runtime — any pair works here.
fn arith_encrypt(data: &[u8], start: u8, step: u8) -> Vec<u8> {
    data.iter()
        .enumerate()
        .map(|(i, &b)| b ^ start.wrapping_add(((i % 32) as u8).wrapping_mul(step)))
        .collect()
}

fn xor_catalog(data: &[u8]) -> Vec<u8> {
    arith_encrypt(data, 0xa3, 0xa0) // start=0xa3, step=0xa0 (period 4)
}

fn xor_sources(data: &[u8]) -> Vec<u8> {
    arith_encrypt(data, 0x51, 0x81) // start=0x51, step=0x81 (period 256)
}

// ---- search ----

#[tokio::test]
async fn search_decrypts_catalog_and_maps_fields() {
    let server = MockServer::start().await;
    let plain = r#"{"data":[{"id":"279","title":"Tamako Love Story","poster_url":"https://img.example.com/p.jpg","season_year":2014}],"meta":{"total":1,"page":1,"per_page":32,"total_pages":1}}"#;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/catalog/search"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(xor_catalog(plain.as_bytes())),
        )
        .mount(&server)
        .await;

    let results = provider_for(&server).search("tamako").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id.0, "279");
    assert_eq!(results[0].title, "Tamako Love Story");
    assert_eq!(results[0].year, Some(2014));
}

#[tokio::test]
async fn search_empty_query_uses_catalog_path() {
    let server = MockServer::start().await;
    let plain = r#"{"data":[],"meta":{"total":0,"page":1,"per_page":32,"total_pages":0}}"#;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/catalog/search"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(xor_catalog(plain.as_bytes())),
        )
        .mount(&server)
        .await;

    let results = provider_for(&server).search("").await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn search_page_sends_page_and_sort_and_reads_meta() {
    let server = MockServer::start().await;
    let plain = r#"{"data":[{"id":"5","title":"E","season_year":2020}],"meta":{"total":97,"page":3,"per_page":32,"total_pages":4}}"#;
    Mock::given(method("GET"))
        .and(path("/api/catalog/search"))
        .and(query_param("page", "3"))
        .and(query_param("sort", "year_desc"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(xor_catalog(plain.as_bytes())),
        )
        .mount(&server)
        .await;

    let page = provider_for(&server)
        .search_page("", 3, "year_desc")
        .await
        .unwrap();
    assert_eq!(page.page, 3);
    assert_eq!(page.total_pages, 4);
    assert_eq!(page.total, 97);
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn search_page_rejects_unknown_sort() {
    // An unvalidated sort must fall back to the default; the server only sees
    // sort=relevance, never the injected value.
    let server = MockServer::start().await;
    let plain = r#"{"data":[],"meta":{"total":0,"page":1,"per_page":32,"total_pages":1}}"#;
    Mock::given(method("GET"))
        .and(path("/api/catalog/search"))
        .and(query_param("sort", "relevance"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(xor_catalog(plain.as_bytes())),
        )
        .mount(&server)
        .await;

    let page = provider_for(&server)
        .search_page("", 1, "../etc/passwd")
        .await
        .unwrap();
    assert!(page.items.is_empty());
}

// ---- details ----

#[tokio::test]
async fn details_extracts_anime_from_html_script() {
    let server = MockServer::start().await;
    let html = r#"<html><body>
        <script type="application/ld+json">{"@context":"https://schema.org","@type":"TVSeries"}</script>
        <script>{"anime":{"id":279,"title":{"userPreferred":"Tamako Love Story","romaji":"Tamako Love Story","english":null,"native":"たまこラブストーリー"},"description":"Tamako loves mochi.","coverImage":{"large":"https://img.example.com/cover.jpg"},"genres":["Romance"],"status":"Ended","episodesList":[{"id":1001,"number":1,"title":"A Maiden's Longing"}]},"watchedEpisodes":[],"currentEpisode":null,"likesCount":100,"dislikesCount":5,"userReaction":null}</script>
    </body></html>"#;
    Mock::given(method("GET"))
        .and(path("/anime/279"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(html),
        )
        .mount(&server)
        .await;

    let details = provider_for(&server)
        .details(&AnimeId("279".into()))
        .await
        .unwrap();
    assert_eq!(details.id.0, "279");
    assert_eq!(details.title, "Tamako Love Story");
    assert_eq!(details.status.as_deref(), Some("Ended"));
    assert_eq!(details.episodes.len(), 1);
    assert_eq!(details.episodes[0].id.0, "1001");
    assert_eq!(details.episodes[0].number, "1");
    assert_eq!(details.episodes[0].title.as_deref(), Some("A Maiden's Longing"));
}

#[tokio::test]
async fn details_page_error_surfaces_as_provider_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/anime/500"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let err = provider_for(&server)
        .details(&AnimeId("500".into()))
        .await
        .expect_err("503 should surface");
    assert!(matches!(err, Error::Provider(_)), "got {err:?}");
}

// ---- resolve ----

#[tokio::test]
async fn resolve_decrypts_sources_and_maps_fields() {
    let server = MockServer::start().await;
    // ensure_session() does a best-effort GET to "/" — serve minimal HTML so it succeeds.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html></html>"))
        .mount(&server)
        .await;
    let plain = r#"[{"id":1001,"url":"https://vidmoly.biz/embed-abc123.html","host":"vidmoly","language":"VOSTFR","episodeId":9001}]"#;
    Mock::given(method("POST"))
        .and(path("/api/sources/anime"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(xor_sources(plain.as_bytes())),
        )
        .mount(&server)
        .await;

    let sources = provider_for(&server)
        .resolve(&AnimeId("1".into()), &EpisodeId("9001".into()))
        .await
        .unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].url, "https://vidmoly.biz/embed-abc123.html");
    assert!(sources[0]
        .http_headers
        .iter()
        .any(|(k, _)| k == "Referer"));
}

// ---- validation (no HTTP requests made) ----

#[tokio::test]
async fn non_numeric_anime_id_is_rejected_before_any_request() {
    let server = MockServer::start().await;
    let err = provider_for(&server)
        .details(&AnimeId("../etc/passwd".into()))
        .await
        .expect_err("hostile id must be rejected");
    assert!(matches!(err, Error::InvalidUrl(_)), "got {err:?}");
}

#[tokio::test]
async fn resolve_validates_episode_id() {
    let server = MockServer::start().await;
    let err = provider_for(&server)
        .resolve(&AnimeId("1".into()), &EpisodeId("nope".into()))
        .await
        .expect_err("non-numeric episode id must be rejected");
    assert!(matches!(err, Error::InvalidUrl(_)), "got {err:?}");
}

#[tokio::test]
async fn resolve_validates_anime_id() {
    let server = MockServer::start().await;
    let err = provider_for(&server)
        .resolve(&AnimeId("bad/id".into()), &EpisodeId("1".into()))
        .await
        .expect_err("non-numeric anime id must be rejected");
    assert!(matches!(err, Error::InvalidUrl(_)), "got {err:?}");
}
