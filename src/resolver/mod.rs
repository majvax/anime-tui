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

#[cfg(test)]
mod tests {
    use super::*;

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
