//! Source resolution: turning an episode/embed page into a concrete, validated
//! playable stream. Kept separate from providers so resolution logic (embeds,
//! token dances, quality enumeration) can be reused and tested in isolation.
//!
//! SECURITY: `validate_stream_url` is the single choke point every URL must pass
//! before it is handed to mpv. mpv is always spawned with an argument array
//! (never a shell), so the remaining risk is protocol/host abuse — rejected here.

use crate::errors::{Error, Result};
use url::Url;

/// Protocols we are willing to hand to mpv. Anything else (file over a remote
/// resolve, `javascript:`, `data:`, etc.) is rejected.
const ALLOWED_SCHEMES: &[&str] = &["http", "https"];

/// Validate a stream URL before playback. Returns the normalized URL string.
pub fn validate_stream_url(raw: &str) -> Result<String> {
    let url = Url::parse(raw).map_err(|_| Error::InvalidUrl(raw.to_string()))?;

    if !ALLOWED_SCHEMES.contains(&url.scheme()) {
        return Err(Error::InvalidUrl(format!(
            "scheme `{}` is not allowed",
            url.scheme()
        )));
    }
    if url.host_str().is_none() {
        return Err(Error::InvalidUrl("missing host".into()));
    }
    Ok(url.to_string())
}

/// Host of the sibnet video CDN; direct stream paths (`/v/…`) are relative to it,
/// and it must be sent as the `Referer` or the CDN 403s.
pub const SIBNET_BASE: &str = "https://video.sibnet.ru";

/// Extract the direct video URL from a sibnet `shell.php` embed page.
///
/// The page's player is initialised as `player.src([{src: "/v/xxxx/nnnn.mp4", …}])`
/// (occasionally an absolute URL). We take the first `/v/…` path (or absolute
/// http(s) `…/v/…` URL) and resolve it against [`SIBNET_BASE`]. Pure/fixture-tested;
/// the HTTP fetch + `Referer` handling lives in the async layer.
pub fn sibnet_direct_url(html: &str) -> Option<String> {
    // Find the first quoted string containing "/v/" …
    let marker = "/v/";
    let anchor = html.find(marker)?;
    // Walk back to the opening quote of the string literal.
    let before = &html[..anchor];
    let quote = before.rfind(['"', '\''])?;
    let quote_char = before.as_bytes()[quote] as char;
    let start = quote + 1;
    let rest = &html[start..];
    let end = rest.find(quote_char)?;
    let path = rest[..end].trim();
    if !path.contains("/v/") {
        return None;
    }
    if path.starts_with("http://") || path.starts_with("https://") {
        Some(path.to_string())
    } else if path.starts_with('/') {
        Some(format!("{SIBNET_BASE}{path}"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibnet_extracts_relative_path() {
        let html = r#"<script>var player=new Playerjs({file:"..."});
            player.src([{src: "/v/e1/2233445.mp4", type:"video/mp4"}]);</script>"#;
        assert_eq!(
            sibnet_direct_url(html).as_deref(),
            Some("https://video.sibnet.ru/v/e1/2233445.mp4"),
        );
    }

    #[test]
    fn sibnet_extracts_absolute_url() {
        let html = r#"player.src([{src: 'https://cdn.sibnet.ru/v/xy/9.mp4'}])"#;
        assert_eq!(
            sibnet_direct_url(html).as_deref(),
            Some("https://cdn.sibnet.ru/v/xy/9.mp4"),
        );
    }

    #[test]
    fn sibnet_none_when_absent() {
        assert!(sibnet_direct_url("<html>no source here</html>").is_none());
    }

    #[test]
    fn accepts_https() {
        assert!(validate_stream_url("https://cdn.example.com/v.m3u8").is_ok());
    }

    #[test]
    fn rejects_non_http_schemes() {
        for bad in [
            "javascript:alert(1)",
            "data:text/html,x",
            "file:///etc/passwd",
            "ftp://example.com/x",
        ] {
            assert!(validate_stream_url(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(validate_stream_url("not a url").is_err());
    }
}
