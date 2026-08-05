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

/// Direct media URL from a player embed page: the plain `sources:[{file:"…"}]` URL
/// if present, else the one hidden inside p.a.c.k.e.r-packed JS. Covers the many
/// jwplayer-style hosts (vidmoly, lulustream/luluvdo, smoothpre, vidzy, …).
pub fn packed_stream_url(html: &str) -> Option<String> {
    if let Some(u) = find_media_url(html) {
        return Some(u);
    }
    let start = html.find("eval(function(p,a,c,k,e,d)")?;
    let unpacked = unpack_packed_js(&html[start..])?;
    find_media_url(&unpacked)
}

/// Kept for the vidmoly host wiring; same behaviour as [`packed_stream_url`].
pub fn vidmoly_stream_url(html: &str) -> Option<String> {
    packed_stream_url(html)
}

/// Unpack Dean Edwards' p.a.c.k.e.r output:
/// `}('payload', radix, count, 'k1|k2|…'.split('|'))`. Each base-`radix` token in the
/// payload is replaced by its keyword. Returns the decoded JS, or `None` if `src`
/// isn't packer output.
pub fn unpack_packed_js(src: &str) -> Option<String> {
    let sp = src.find(".split('|')").or_else(|| src.find(".split(\"|\")"))?;
    let call = src[..sp].rfind("}(")?;
    let args = &src[call + 2..sp];
    let bytes = args.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    // Payload: the first quoted string, decoding backslash escapes.
    let mut payload = String::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                payload.push(bytes[i + 1] as char);
                i += 2;
            }
            c if c == quote => {
                i += 1;
                break;
            }
            c => {
                payload.push(c as char);
                i += 1;
            }
        }
    }
    let rest = &args[i..];
    let radix: u32 = rest
        .split(|c: char| !c.is_ascii_digit())
        .find(|t| !t.is_empty())?
        .parse()
        .ok()?;
    // Keywords: between the first and last quote of the remaining args.
    let kwo = rest.find(['\'', '"'])?;
    let kwc = rest.rfind(['\'', '"'])?;
    if kwc <= kwo {
        return None;
    }
    let keywords: Vec<&str> = rest[kwo + 1..kwc].split('|').collect();
    // Single pass over the ORIGINAL payload: each base-`radix` word-token is
    // replaced by its keyword, everything else copied verbatim. This is the correct
    // p.a.c.k.e.r decode — it must NOT iterate `.replace()` over the growing output
    // (a keyword can contain a token substring, which re-expands and blows the
    // string up exponentially: a real vidzy page exploded 7 KB → 7 MB and hung).
    let bytes = payload.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = String::with_capacity(payload.len() * 2);
    let mut i = 0;
    while i < bytes.len() {
        if is_word(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_word(bytes[i]) {
                i += 1;
            }
            let word = &payload[start..i];
            match from_base(word, radix) {
                Some(idx) if idx < keywords.len() && !keywords[idx].is_empty() => {
                    out.push_str(keywords[idx])
                }
                _ => out.push_str(word),
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Some(out)
}

/// Parse a lowercase base-`radix` token to its index — case-sensitive, mirroring
/// JS `c.toString(radix)` (tokens are lowercase; an uppercase word is a real
/// identifier, not a token). Returns `None` if any char is out of range or the
/// value overflows `usize` (a long identifier that isn't a token).
fn from_base(word: &str, radix: u32) -> Option<usize> {
    let mut n: usize = 0;
    for c in word.chars() {
        let d = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 10,
            _ => return None,
        };
        if d >= radix {
            return None;
        }
        n = n.checked_mul(radix as usize)?.checked_add(d as usize)?;
    }
    Some(n)
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

/// The first quoted absolute embed URL (`http(s)://…/e/…`) in the page — the target
/// of VOE's JS `window.location`/mirror redirect stub. Only meaningful on a stub
/// (a real page resolves via [`voe_stream_url`] before we look here).
pub fn voe_mirror_target(html: &str) -> Option<String> {
    let mut search = html;
    while let Some(p) = search.find("http") {
        let tail = &search[p..];
        let end = tail.find(['\'', '"', ' ', '\\', '\n', '\r', '<', ')']).unwrap_or(tail.len());
        let cand = &tail[..end];
        if (cand.starts_with("http://") || cand.starts_with("https://")) && cand.contains("/e/") {
            return Some(cand.to_string());
        }
        search = &tail[4.min(tail.len())..];
    }
    None
}

/// The `<form>` fields of VOE's "Confirm you're human" proof-of-work gate page.
#[derive(Debug, Clone)]
pub struct VoeGate {
    /// Form POST target (also the page's own URL).
    pub action: String,
    /// Laravel CSRF `_token`.
    pub token: String,
    /// URL that returns the PBKDF2 challenge JSON.
    pub challenge_url: String,
}

/// Parse VOE's PoW gate page, or `None` if it isn't one. Presence of `altcha-widget`
/// marks the gate; we then read the form action, CSRF `_token`, and challenge URL.
pub fn voe_gate(html: &str) -> Option<VoeGate> {
    if !html.contains("altcha-widget") {
        return None;
    }
    Some(VoeGate {
        action: quoted_after(html, "action=\"")?,
        token: quoted_after(html, "name=\"_token\" value=\"")?,
        challenge_url: quoted_after(html, "challenge=\"")?,
    })
}

/// The string between `marker` and the next `"`.
fn quoted_after(html: &str, marker: &str) -> Option<String> {
    let at = html.find(marker)? + marker.len();
    let rest = &html[at..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Solve VOE's "confirm you're human" PBKDF2 proof-of-work from the challenge JSON,
/// returning the base64 `altcha` form value to POST back. The PoW: find the smallest
/// counter where PBKDF2-HMAC-SHA256(`nonce ++ big-endian u32(counter)`, `salt`,
/// `cost` iterations, `keyLength` bytes) begins with `keyPrefix`. keyPrefix is one
/// byte ("00"), so ~256 tries typical. Returns `None` on malformed JSON, or if no
/// solution is found within a safety bound.
pub fn voe_solve_challenge(challenge_json: &str) -> Option<String> {
    use ring::pbkdf2;
    let v: serde_json::Value = serde_json::from_str(challenge_json).ok()?;
    let p = v.get("parameters")?;
    let signature = v.get("signature")?;
    let nonce = hex_to_bytes(p.get("nonce")?.as_str()?)?;
    let salt = hex_to_bytes(p.get("salt")?.as_str()?)?;
    let cost = p.get("cost")?.as_u64()? as u32;
    let key_length = p.get("keyLength").and_then(|x| x.as_u64()).unwrap_or(32) as usize;
    let prefix = hex_to_bytes(p.get("keyPrefix")?.as_str()?)?;
    let iters = std::num::NonZeroU32::new(cost)?;

    // password = nonce bytes followed by a big-endian u32 counter (uint32 mode).
    let mut password = nonce;
    let base = password.len();
    password.extend_from_slice(&[0u8; 4]);
    let mut out = vec![0u8; key_length.max(1)];
    let mut counter: u32 = 0;
    let derived_hex = loop {
        password[base..].copy_from_slice(&counter.to_be_bytes());
        pbkdf2::derive(pbkdf2::PBKDF2_HMAC_SHA256, iters, &salt, &password, &mut out);
        if out.starts_with(&prefix) {
            break bytes_to_hex(&out);
        }
        // keyPrefix is 1 byte; a solution is overwhelmingly likely well before this.
        counter = counter.checked_add(1).filter(|c| *c < (1 << 24))?;
    };

    // The server accepts the "verbose" altcha payload: the echoed challenge plus the
    // solution. It verifies by value (order/spacing-independent), confirmed live.
    let payload = serde_json::json!({
        "challenge": { "parameters": p, "signature": signature },
        "solution": { "counter": counter, "derivedKey": derived_hex, "time": 100 }
    });
    Some(base64::engine::general_purpose::STANDARD.encode(serde_json::to_string(&payload).ok()?))
}

fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn bytes_to_hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
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
    fn unpack_packed_js_substitutes_tokens() {
        // }('0 1', 36, 2, 'hello|world'.split('|'))  →  "hello world"
        let src = r"eval(function(p,a,c,k,e,d){}('0 1',36,2,'hello|world'.split('|')))";
        assert_eq!(unpack_packed_js(src).as_deref(), Some("hello world"));
    }

    #[test]
    fn unpack_is_single_pass_no_cascade() {
        // keyword[1] = "(0)" contains a bounded copy of token "0". Single-pass decode
        // emits it verbatim → "(0)". The old iterative decode re-expanded that inner
        // "0" into keyword[0]="X" → "(X)", and on real pages this cascade blew the
        // string up exponentially (7 KB → 7 MB) and hung. Guard against regressing.
        let src = r"eval(function(p,a,c,k,e,d){}('1',2,2,'X|(0)'.split('|')))";
        assert_eq!(unpack_packed_js(src).as_deref(), Some("(0)"));
    }

    #[test]
    fn packed_stream_url_finds_hls_in_packed_js() {
        // Like real pages, the URL is assembled from separate tokens so no complete
        // ".m3u8" exists in the raw payload. Payload `0:"1.2"` with keywords
        // [file, https://cdn/x/master, m3u8] → `file:"https://cdn/x/master.m3u8"`.
        let src = r#"eval(function(p,a,c,k,e,d){}('0:"1.2"',36,3,'file|https://cdn/x/master|m3u8'.split('|')))"#;
        assert_eq!(
            packed_stream_url(src).as_deref(),
            Some("https://cdn/x/master.m3u8"),
        );
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
    fn voe_mirror_target_extracts_stub_redirect() {
        let stub = r#"<script>window.location.href = 'https://othermirror.com/e/abc123';</script>"#;
        assert_eq!(
            voe_mirror_target(stub).as_deref(),
            Some("https://othermirror.com/e/abc123"),
        );
        assert!(voe_mirror_target("<html>no redirect</html>").is_none());
    }

    #[test]
    fn voe_gate_parses_form_fields() {
        let html = r#"<form method="POST" action="https://m.com/e/xy" class="access-form">
            <input type="hidden" name="_token" value="CSRF123">
            <altcha-widget challenge="https://m.com/chal.json" hidefooter></altcha-widget>
            </form>"#;
        let g = voe_gate(html).expect("gate");
        assert_eq!(g.action, "https://m.com/e/xy");
        assert_eq!(g.token, "CSRF123");
        assert_eq!(g.challenge_url, "https://m.com/chal.json");
        // A normal page (no widget) is not a gate.
        assert!(voe_gate("<html>video</html>").is_none());
    }

    #[test]
    fn voe_pow_solution_satisfies_challenge() {
        // cost=1 keeps the test fast; keyPrefix "00" ⇒ ~256 tries.
        let challenge = r#"{"parameters":{"algorithm":"PBKDF2/SHA-256","cost":1,"keyLength":32,"keyPrefix":"00","nonce":"aabbccdd","salt":"11223344"},"signature":"deadbeef"}"#;
        let b64 = voe_solve_challenge(challenge).expect("solve");
        let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["challenge"]["signature"], "deadbeef");
        let counter = v["solution"]["counter"].as_u64().unwrap() as u32;
        // Re-derive and confirm the proof-of-work actually holds.
        let mut pw = vec![0xaa, 0xbb, 0xcc, 0xdd];
        pw.extend_from_slice(&counter.to_be_bytes());
        let mut out = [0u8; 32];
        ring::pbkdf2::derive(
            ring::pbkdf2::PBKDF2_HMAC_SHA256,
            std::num::NonZeroU32::new(1).unwrap(),
            &[0x11, 0x22, 0x33, 0x44],
            &pw,
            &mut out,
        );
        assert_eq!(out[0], 0x00, "derived key must start with keyPrefix");
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
