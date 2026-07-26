//! Real, observed Nakanime transport facts (host: nakanime.tv), confirmed
//! 2026-07-25. See docs/NAKANIME_RECON.md for how these were established.
//!
//! Stack: Cloudflare → AdonisJS + Inertia single-page app.
//!
//! Everything site-specific lives in this file so a maintainer touches one place.

/// Catalogue search. `GET /api/catalog/search?sort=relevance&per_page=32&page=N[&keyword=Q]`
/// Response: XOR-encrypted `application/octet-stream`; key: `[0xdf, 0x9f, 0x5f, 0x1f]` repeating.
pub const CATALOG_SEARCH_PATH: &str = "/api/catalog/search";
pub const CATALOG_SORT_DEFAULT: &str = "relevance";
pub const CATALOG_PER_PAGE: u32 = 32;
pub const CATALOG_KEYWORD_PARAM: &str = "q";

/// Anime detail HTML page. `GET /anime/{id}` → 302 → `/anime/{id}/{slug}` (reqwest follows
/// redirect automatically). Anime JSON is embedded in an inline `<script>` tag as
/// `{"anime": {...}, "watchedEpisodes": [...], ...}`.
pub const ANIME_PAGE_PATH: &str = "/anime/{id}";

/// Episode sources. `POST /api/sources/anime` with JSON body
/// `{"anime_id": N, "episode_id": N, "title": "", "turnstile_token": ""}`.
/// Response: XOR-encrypted; 32-byte key generated as `(0x51 + j) | (0x80 * (j & 1))` for j=0..31.
pub const SOURCES_PATH: &str = "/api/sources/anime";

/// Embed resolver. `POST /api/proxy/resolve-embed` with
/// `{"sourceId": N, "host": "...", "url": "..."}`.
/// Returns `{"proxied_url": "..."}` on success or 204 No Content when unavailable.
/// Response encryption key: undetermined (endpoint typically returns 204 in testing).
pub const RESOLVE_EMBED_PATH: &str = "/api/proxy/resolve-embed";

/// Plain-JSON health endpoints (not encrypted).
pub const ANNOUNCEMENTS_PATH: &str = "/api/announcements/active";
pub const FAI_STATUS_PATH: &str = "/api/fai-status";

/// Build a path by substituting `{id}` with a (validated, numeric) id.
pub fn with_id(template: &str, id: &str) -> String {
    template.replace("{id}", id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_substitution() {
        assert_eq!(with_id(ANIME_PAGE_PATH, "279"), "/anime/279");
        assert_eq!(with_id(SOURCES_PATH, "42"), "/api/sources/anime");
    }
}
