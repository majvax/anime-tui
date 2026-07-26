//! Nakanime provider (host: nakanime.tv).
//!
//! Transport is real and confirmed (see `docs/NAKANIME_RECON.md`):
//! AdonisJS + Inertia SPA behind Cloudflare.
//!
//! ## Encryption
//!
//! API responses (`/api/catalog/search`, `/api/sources/anime`) are XOR-
//! encrypted with a per-session arithmetic key: `key[i] = (start + i*step) % 256`.
//! `start` and `step` are recovered at runtime via a 2-byte known-plaintext
//! attack against the fixed JSON prefix (`{"data":[` for catalog, `[{` for
//! sources). This is self-healing — no hardcoded key is needed.
//!
//! ## Anime details
//!
//! The detail page (`/anime/{id}`) is server-rendered HTML; the full anime
//! JSON is embedded in an inline `<script>` tag as `{"anime": {...}, ...}`.
//! No separate `/api/anime/{id}` call is needed.

pub mod endpoints;
pub mod parse;

use crate::config::Config;
use crate::errors::{provider_changed, Error, Result};
use crate::models::*;
use async_trait::async_trait;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{ACCEPT, CONTENT_TYPE, REFERER};
use reqwest::Client;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use super::Provider;

/// Decrypt (or pass through) an XOR-encrypted API response.
///
/// The cipher is `key[i] = (start + i * step) % 256`. Both `start` and `step`
/// are recovered via a 2-byte known-plaintext attack using `known_prefix`
/// (the fixed JSON prefix the server always emits). If the body already starts
/// with the prefix it is returned as-is (server sent plain JSON).
fn dynamic_decrypt(context: &str, body: &[u8], known_prefix: &[u8]) -> Result<String> {
    if body.len() < 2 {
        return Err(provider_changed(context, "response body too short"));
    }
    // Already plain JSON?
    if body.starts_with(known_prefix) {
        return String::from_utf8(body.to_vec())
            .map_err(|_| provider_changed(context, "response is not valid UTF-8"));
    }
    // Recover key from first two bytes.
    let start = body[0] ^ known_prefix[0];
    let step = (body[1] ^ known_prefix[1]).wrapping_sub(start);
    // The server generates a 32-byte key block and repeats it.
    // Use i % 32 so decryption matches that period.
    let plain: Vec<u8> = body
        .iter()
        .enumerate()
        .map(|(i, &b)| b ^ start.wrapping_add(((i % 32) as u8).wrapping_mul(step)))
        .collect();
    String::from_utf8(plain).map_err(|utf8_err| {
        let attempted = utf8_err.into_bytes();
        let _ = std::fs::write(format!("/tmp/nakanime_{context}_raw.bin"), body);
        let _ = std::fs::write(format!("/tmp/nakanime_{context}_decrypted.bin"), &attempted);
        let raw_hex: String = body[..body.len().min(32)]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        provider_changed(context, format!("decryption failed — raw: {raw_hex}"))
    })
}

/// Minimal percent-decode for cookie values (handles %XX sequences only).
fn percent_decode(s: &str) -> String {
    let mut result = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            match (bytes.next(), bytes.next()) {
                (Some(h), Some(l)) => {
                    if let Ok(hex) = std::str::from_utf8(&[h, l]) {
                        if let Ok(byte) = u8::from_str_radix(hex, 16) {
                            result.push(byte);
                            continue;
                        }
                    }
                    result.push(b'%');
                    result.push(h);
                    result.push(l);
                }
                _ => result.push(b'%'),
            }
        } else {
            result.push(b);
        }
    }
    String::from_utf8_lossy(&result).into_owned()
}

pub struct Nakanime {
    client: Client,
    jar: Arc<Jar>,
    base_url: String,
    session_ready: AtomicBool,
}

impl Nakanime {
    /// Build a provider pointing at `base_url`. The caller is responsible for
    /// building the client with `cookie_provider(Arc::clone(&jar))` if they
    /// need XSRF cookie extraction. For tests, `Client::new()` is fine.
    pub fn new(client: Client, base_url: impl Into<String>) -> Self {
        Self {
            client,
            jar: Arc::new(Jar::default()),
            base_url: normalize_base(base_url.into()),
            session_ready: AtomicBool::new(false),
        }
    }

    /// Build a provider from config with a cookie store and browser-like headers.
    pub fn from_config(config: &Config) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let client = Client::builder()
            .user_agent(&config.network.user_agent)
            .timeout(std::time::Duration::from_secs(config.network.timeout_secs))
            .cookie_provider(Arc::clone(&jar))
            .build()
            .map_err(|e| Error::Network(e.to_string()))?;
        Ok(Self {
            client,
            jar,
            base_url: normalize_base(config.base_url.clone()),
            session_ready: AtomicBool::new(false),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Read the XSRF-TOKEN cookie from the jar (set by the site on first GET).
    fn xsrf_token(&self) -> String {
        let Ok(base_url) = url::Url::parse(&self.base_url) else {
            return String::new();
        };
        let Some(hval) = self.jar.cookies(&base_url) else {
            return String::new();
        };
        let Ok(cookie_str) = hval.to_str() else {
            return String::new();
        };
        for part in cookie_str.split(';') {
            let part = part.trim();
            if let Some(val) = part.strip_prefix("XSRF-TOKEN=") {
                return percent_decode(val);
            }
        }
        String::new()
    }

    /// GET a path and return the raw response bytes (for XOR-encrypted API endpoints).
    /// Retries up to 3 times on transient network failures.
    async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let url = self.url(path);
        let mut last: Option<Error> = None;
        for attempt in 0..3u32 {
            let resp = self
                .client
                .get(&url)
                .header(ACCEPT, "application/json, text/plain, */*")
                .header(REFERER, &self.base_url)
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    return r
                        .bytes()
                        .await
                        .map(|b| b.to_vec())
                        .map_err(|e| Error::Network(e.to_string()));
                }
                Ok(r) => {
                    return Err(Error::Provider(format!("unexpected status {}", r.status())));
                }
                Err(e) if e.is_timeout() => last = Some(Error::Timeout),
                Err(e) => last = Some(Error::Network(e.to_string())),
            }
            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(
                    200 * u64::from(attempt + 1),
                ))
                .await;
            }
        }
        Err(last.unwrap_or_else(|| Error::Network("request failed".into())))
    }

    /// GET a path and return the HTML body (for page scraping). Follows redirects.
    async fn get_html(&self, path: &str) -> Result<String> {
        let url = self.url(path);
        let resp = self
            .client
            .get(&url)
            .header(ACCEPT, "text/html,application/xhtml+xml,*/*")
            .header(REFERER, &self.base_url)
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(Error::Provider(format!(
                "page returned status {}",
                resp.status()
            )));
        }
        resp.text()
            .await
            .map_err(|e| Error::Network(e.to_string()))
    }

    /// POST a JSON body and return `(status_code, response_bytes)`.
    /// Includes `X-XSRF-TOKEN` header when a token is available.
    async fn post_bytes(&self, path: &str, body: &str) -> Result<(u16, Vec<u8>)> {
        let url = self.url(path);
        let xsrf = self.xsrf_token();
        let mut builder = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(REFERER, &self.base_url)
            .header("X-Requested-With", "XMLHttpRequest")
            .body(body.to_string());
        if !xsrf.is_empty() {
            builder = builder.header("X-XSRF-TOKEN", xsrf);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;
        Ok((status, bytes.to_vec()))
    }

    /// Visit the site root to initialise session cookies (XSRF-TOKEN, adonis-session).
    /// Best-effort: errors are silently ignored so callers don't fail due to cookie init.
    async fn ensure_session(&self) {
        if !self.session_ready.load(Ordering::Relaxed) {
            let _ = self.get_html("/").await;
            self.session_ready.store(true, Ordering::Relaxed);
        }
    }
}

#[async_trait]
impl Provider for Nakanime {
    fn name(&self) -> &'static str {
        "nakanime"
    }

    async fn search(&self, query: &str) -> Result<Vec<AnimeSummary>> {
        let path = if query.is_empty() {
            format!(
                "{}?sort={}&per_page={}&page=1",
                endpoints::CATALOG_SEARCH_PATH,
                endpoints::CATALOG_SORT_DEFAULT,
                endpoints::CATALOG_PER_PAGE,
            )
        } else {
            format!(
                "{}?sort={}&per_page={}&page=1&{}={}",
                endpoints::CATALOG_SEARCH_PATH,
                endpoints::CATALOG_SORT_DEFAULT,
                endpoints::CATALOG_PER_PAGE,
                endpoints::CATALOG_KEYWORD_PARAM,
                urlencode(query),
            )
        };
        let body = self.get_bytes(&path).await?;
        let json = dynamic_decrypt("catalog", &body, b"{\"data\":[")?;
        parse::parse_catalog(&json)
    }

    async fn details(&self, id: &AnimeId) -> Result<AnimeDetails> {
        validate_numeric_id(&id.0)?;
        let html = self
            .get_html(&endpoints::with_id(endpoints::ANIME_PAGE_PATH, &id.0))
            .await?;
        parse::parse_anime_details_from_html(&html)
    }

    async fn episodes(&self, id: &AnimeId) -> Result<Vec<Episode>> {
        Ok(self.details(id).await?.episodes)
    }

    async fn resolve(&self, anime: &AnimeId, episode: &EpisodeId) -> Result<Vec<PlayableSource>> {
        validate_numeric_id(&anime.0)?;
        validate_numeric_id(&episode.0)?;
        self.ensure_session().await;

        let body = serde_json::json!({
            "anime_id": anime.0.parse::<u64>().unwrap_or(0),
            "episode_id": episode.0.parse::<u64>().unwrap_or(0),
            "title": "",
            "turnstile_token": ""
        })
        .to_string();

        let (status, bytes) = self.post_bytes(endpoints::SOURCES_PATH, &body).await?;
        if status != 200 {
            return Err(Error::Provider(format!(
                "sources endpoint returned status {status}"
            )));
        }

        let json = dynamic_decrypt("sources", &bytes, b"[{")?;
        parse::parse_episode_sources(&json)
    }
}

fn normalize_base(mut base: String) -> String {
    while base.ends_with('/') {
        base.pop();
    }
    base
}

fn validate_numeric_id(id: &str) -> Result<()> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(Error::InvalidUrl(format!("non-numeric nakanime id: {id:?}")))
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make() -> Nakanime {
        Nakanime::new(Client::new(), "https://nakanime.tv/")
    }

    #[test]
    fn base_url_is_normalized() {
        let n = make();
        assert_eq!(n.url("/api/catalog/search"), "https://nakanime.tv/api/catalog/search");
    }

    #[test]
    fn numeric_ids_only() {
        assert!(validate_numeric_id("279").is_ok());
        assert!(validate_numeric_id("../secrets").is_err());
        assert!(validate_numeric_id("").is_err());
        assert!(validate_numeric_id("279/watch").is_err());
    }

    #[test]
    fn urlencode_escapes_safely() {
        assert_eq!(urlencode("naruto shippuden"), "naruto+shippuden");
        assert_eq!(urlencode("a/b?c"), "a%2Fb%3Fc");
    }

    fn arith_encrypt(plain: &[u8], start: u8, step: u8) -> Vec<u8> {
        plain.iter().enumerate()
            .map(|(i, &b)| b ^ start.wrapping_add(((i % 32) as u8).wrapping_mul(step)))
            .collect()
    }

    #[test]
    fn dynamic_decrypt_passes_through_plain_json() {
        let plain = r#"{"data":[]}"#;
        let result = dynamic_decrypt("test", plain.as_bytes(), b"{\"data\":").unwrap();
        assert_eq!(result, plain);
    }

    #[test]
    fn dynamic_decrypt_recovers_arithmetic_key() {
        let plain = r#"{"data":[],"meta":{}}"#;
        // use start=0xa3, step=0xa0 (matches old hardcoded catalog key)
        let enc = arith_encrypt(plain.as_bytes(), 0xa3, 0xa0);
        let result = dynamic_decrypt("test", &enc, b"{\"data\":").unwrap();
        assert_eq!(result, plain);
    }

    #[test]
    fn dynamic_decrypt_handles_step_zero() {
        // step=0 means single-byte repeating XOR
        let plain = r#"{"data":[]}"#;
        let enc = arith_encrypt(plain.as_bytes(), 0x42, 0x00);
        let result = dynamic_decrypt("test", &enc, b"{\"data\":").unwrap();
        assert_eq!(result, plain);
    }

    #[test]
    fn dynamic_decrypt_sources_prefix() {
        let plain = r#"[{"id":1,"url":"https://example.com"}]"#;
        let enc = arith_encrypt(plain.as_bytes(), 0x51, 0x81);
        let result = dynamic_decrypt("test", &enc, b"[{").unwrap();
        assert_eq!(result, plain);
    }

    #[test]
    fn dynamic_decrypt_key_resets_at_32() {
        // Verify that the 32-byte period is respected.
        // With step=0x81 (odd), i*step mod 256 would NOT be periodic-32 without i%32.
        // This test uses a plaintext longer than 32 bytes to exercise the wrap.
        let plain = r#"[{"id":9999,"url":"https://vidmoly.biz/embed-abcdefghijk.html"}]"#;
        assert!(plain.len() > 32, "test requires >32 bytes");
        let enc = arith_encrypt(plain.as_bytes(), 0x51, 0x81);
        let result = dynamic_decrypt("test", &enc, b"[{").unwrap();
        assert_eq!(result, plain);
    }

    #[test]
    fn percent_decode_handles_cookie_values() {
        assert_eq!(percent_decode("e%3Afoo"), "e:foo");
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("plain"), "plain");
    }
}
