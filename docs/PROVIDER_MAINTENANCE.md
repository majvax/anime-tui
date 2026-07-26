# Provider maintenance (Nakanime)

## Status: fully implemented ✓

The Nakanime provider is complete. All three pipeline stages work end-to-end:
fetch → XOR-decrypt → parse. Integration tests cover each stage with mock HTTP
(wiremock); parsers are tested with inline fixtures.

## Module map

- `endpoints.rs` — confirmed endpoint paths, keys, and param names. **Edit only
  this file** when URLs or query params change.
- `mod.rs` — `reqwest` client (cookie store, browser headers), numeric-id
  validation, bounded retries, XOR-decrypt, `ensure_session` (lazy cookie init
  via GET `/`), and the full `Provider` trait implementation.
- `parse.rs` — pure parsers: `parse_catalog`, `parse_anime_details`
  (from HTML script tag JSON), `parse_episode_sources`. Zero IO, fully tested.

## Encryption

Both encrypted endpoints use simple repeating XOR (not AES):

| Endpoint | Key |
|----------|-----|
| `GET /api/catalog/search` | `[0xdf, 0x9f, 0x5f, 0x1f]` (4-byte repeating) |
| `POST /api/sources/anime` | 32-byte: `key[j] = (0x51 + j) \| (0x80 * (j & 1))` |

Keys are static and session-independent.

## Data flow

```
search(query)
  GET /api/catalog/search?sort=relevance&per_page=32&page=1[&keyword=...]
  → XOR-decrypt (catalog key) → parse_catalog → Vec<AnimeSummary>

details(AnimeId)
  GET /anime/{id}  (follows redirect to /anime/{id}/{slug})
  → scraper finds <script>{"anime":{...}} → parse_anime_details → AnimeDetails
  (episodes come from anime.episodesList; EpisodeId = nakanime internal ID)

episodes(AnimeId)
  → calls details(), returns .episodes

resolve(AnimeId, EpisodeId)
  → ensure_session() [GET / to initialise XSRF cookie, best-effort]
  POST /api/sources/anime {anime_id, episode_id, title:"", turnstile_token:""}
  → XOR-decrypt (sources key) → parse_episode_sources → Vec<PlayableSource>
  (URLs are embed pages; mpv with yt-dlp resolves them to HLS)
```

## When things break

| Symptom | Fix |
|---------|-----|
| `ProviderChanged` in `catalog` | URL or query params changed → update `endpoints.rs` |
| `ProviderChanged` in `anime_detail` | HTML structure changed → update `extract_anime_json_from_html` in `parse.rs` |
| `ProviderChanged` in `sources` | POST endpoint changed → check `SOURCES_PATH` and request body |
| XOR decrypt produces garbage | Key may have rotated → re-capture with browser JSON.parse hook |
| Sources POST returns non-200 | May need valid XSRF-TOKEN; check `ensure_session` flow |

## Adding resolve-embed support

`POST /api/proxy/resolve-embed {sourceId, host, url}` currently returns 204
in testing (server-side resolution fails). If it starts returning data:

1. Capture raw response bytes and decrypted `proxied_url` value via browser hook.
2. XOR raw bytes with known plaintext to recover the key.
3. Add `decrypt_resolve_embed(body)` using that key.
4. In `resolve()`, optionally call resolve-embed per source and replace the
   embed URL with `proxied_url` when non-null.

## Boundaries

Only content the user is authorized to access. Never log cookies, session
tokens, referer, user-agent, or stream URLs. The `http_headers` field of
`PlayableSource` is treated as sensitive; mpv receives it via `--http-header-fields`
and that arg vector is never logged.
