//! Source resolution: turning an episode/embed page into a concrete, validated
//! playable stream. Kept separate from providers so resolution logic (embeds,
//! token dances, quality enumeration) can be reused and tested in isolation.
//!
//! SECURITY: `validate_stream_url` is the single choke point every URL must pass
//! before it is handed to mpv. mpv is always spawned with an argument array
//! (never a shell), so the remaining risk is protocol/host abuse — rejected here.

use base64::Engine as _;
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

/// The first `http(s)://…​.m3u8` or `…​.mp4` URL in `text` (e.g. a jwplayer
/// `sources:[{file:"…"}]`), or `None`. Used by the vidmoly extractor and as a
/// generic last resort. Stops the URL at the first quote/space/backslash.
pub fn find_media_url(text: &str) -> Option<String> {
    let ext_pos = [".m3u8", ".mp4"]
        .iter()
        .filter_map(|e| text.find(e))
        .min()?;
    let start = text[..ext_pos].rfind("http")?;
    let tail = &text[start..];
    let end = tail
        .find(['"', '\'', ' ', '\\', '\n', '\r', ')', '<'])
        .unwrap_or(tail.len());
    let url = tail[..end].to_string();
    (url.starts_with("http") && (url.contains(".m3u8") || url.contains(".mp4"))).then_some(url)
}

/// Extract the direct stream from a vidmoly embed page. Vidmoly serves an HLS
/// playlist referenced as `sources:[{file:"…​.m3u8"}]`; the CDN needs a vidmoly
/// `Referer`. Returns the m3u8 URL (the caller supplies the Referer).
pub fn vidmoly_stream_url(html: &str) -> Option<String> {
    find_media_url(html)
}

fn rot13(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .map(|&b| match b {
            b'a'..=b'z' => (b - b'a' + 13) % 26 + b'a',
            b'A'..=b'Z' => (b - b'A' + 13) % 26 + b'A',
            _ => b,
        })
        .collect()
}

fn b64(bytes: &[u8]) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(bytes)
        .ok()
}

/// Junk digraphs VOE interleaves into its obfuscated payload (stripped before the
/// base64 stages). Confirmed against a live embed page, 2026-07.
const VOE_JUNK: [&str; 7] = ["@$", "^^", "~@", "%?", "*~", "!!", "#&"];

/// Decode VOE's obfuscated payload string (element 0 of the `application/json`
/// array): ROT13 → strip the junk digraphs → base64 → shift each byte by −3 →
/// reverse → base64 → UTF-8 JSON. Returns the decoded JSON string.
///
/// NOTE: VOE rotates its obfuscation periodically; if this stops matching, capture a
/// fresh embed page (ANIME_TUI_DUMP / `cargo run --example dump_voe`) and adjust.
pub fn voe_decode(payload: &str) -> Option<String> {
    let mut s = String::from_utf8(rot13(payload.trim().as_bytes())).ok()?;
    for junk in VOE_JUNK {
        s = s.replace(junk, "");
    }
    let c = b64(s.as_bytes())?;
    let d: Vec<u8> = c.iter().map(|&x| x.wrapping_sub(3)).collect();
    let e: Vec<u8> = d.into_iter().rev().collect();
    let f = b64(&e)?;
    String::from_utf8(f).ok()
}

/// Extract the playable stream URL from a VOE embed page: read the obfuscated payload
/// from `<script type="application/json">["…"]</script>`, [`voe_decode`] it, and take
/// the `source` (HLS) — or `direct_access_url` (progressive mp4) — from the decoded
/// JSON. The caller adds the `Referer`.
pub fn voe_stream_url(html: &str) -> Option<String> {
    let raw = extract_json_script(html)?;
    // The script body is a JSON array whose first element is the obfuscated payload.
    let arr: Vec<String> = serde_json::from_str(raw.trim()).ok()?;
    let payload = arr.into_iter().next()?;
    let json = voe_decode(&payload)?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    v.get("source")
        .or_else(|| v.get("direct_access_url"))
        .and_then(|x| x.as_str())
        .map(str::to_string)
}

/// The text of the first `<script type="application/json">…</script>` block.
fn extract_json_script(html: &str) -> Option<String> {
    let key = "application/json";
    let at = html.find(key)?;
    let after_tag = html[at..].find('>')? + at + 1;
    let end = html[after_tag..].find("</script>")? + after_tag;
    Some(html[after_tag..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_media_url_extracts_hls_and_mp4() {
        assert_eq!(
            find_media_url(r#"sources:[{file:"https://cdn.vidmoly.to/hls/x/master.m3u8"}]"#).as_deref(),
            Some("https://cdn.vidmoly.to/hls/x/master.m3u8"),
        );
        assert_eq!(
            find_media_url(r#"file: 'https://cdn/x.mp4?token=1' "#).as_deref(),
            Some("https://cdn/x.mp4?token=1"),
        );
        assert!(find_media_url("no media here").is_none());
    }

    /// Build a VOE-obfuscated payload from a JSON string (inverse of `voe_decode`),
    /// mirroring the real algorithm confirmed against a live page.
    fn voe_encode(json: &str) -> String {
        let b64 = base64::engine::general_purpose::STANDARD;
        let e = b64.encode(json.as_bytes()); // decode step 6: base64
        let rev: Vec<u8> = e.into_bytes().into_iter().rev().collect(); // step 5: reverse
        let plus3: Vec<u8> = rev.iter().map(|&x| x.wrapping_add(3)).collect(); // step 4: -3
        let b = b64.encode(&plus3); // step 3: base64
        String::from_utf8(rot13(b.as_bytes())).unwrap() // step 2: rot13 (self-inverse)
    }

    #[test]
    fn voe_decode_roundtrips_real_transform() {
        let json = r#"{"source":"https://delivery.voe/hls/master.m3u8?t=abc"}"#;
        // Junk digraphs are stripped by the decoder, so sprinkling them in is a no-op.
        let mut payload = voe_encode(json);
        payload.insert_str(4, "@$");
        payload.push_str("#&");
        assert_eq!(voe_decode(&payload).as_deref(), Some(json));
    }

    #[test]
    fn voe_stream_url_prefers_source_then_direct() {
        let json = r#"{"source":"https://cdn.voe/hls/master.m3u8?t=x","direct_access_url":"https://cdn.voe/f.mp4"}"#;
        let html = format!(r#"<script type="application/json">["{}"]</script>"#, voe_encode(json));
        assert_eq!(
            voe_stream_url(&html).as_deref(),
            Some("https://cdn.voe/hls/master.m3u8?t=x"),
        );
        // Falls back to the progressive mp4 when there's no HLS source.
        let json2 = r#"{"direct_access_url":"https://cdn.voe/f.mp4?t=y"}"#;
        let html2 = format!(r#"<script type="application/json">["{}"]</script>"#, voe_encode(json2));
        assert_eq!(voe_stream_url(&html2).as_deref(), Some("https://cdn.voe/f.mp4?t=y"));
    }

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
