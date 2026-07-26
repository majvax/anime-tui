# Nakanime reconnaissance (recorded 2026-07-25)

Observations from direct HTTP requests and browser-intercepted JSON captures
against `https://nakanime.tv`. These are facts captured from the public site;
they are the basis for `provider::nakanime`.

## Stack

- **Edge:** Cloudflare (`server: cloudflare`, `cf-ray`, `cf-cache-status`).
- **App:** AdonisJS (`adonis-session` cookie) + Vue SPA. Anime detail pages are
  server-rendered HTML; the SPA hydrates on the client side.
- **Cookies set on first load:** `XSRF-TOKEN`, `adonis-session`, `visitor_id`.
  A cookie store is required (provider uses `reqwest` `cookie_provider`).

## Endpoints (confirmed)

| Method | Path | Notes |
|--------|------|-------|
| GET | `/api/catalog/search?sort=relevance&per_page=32&page=N[&keyword=Q]` | XOR-encrypted response |
| GET | `/anime/{id}` | 302 → `/anime/{id}/{slug}`; anime JSON in `<script>` tag |
| POST | `/api/sources/anime` | XOR-encrypted response; body: `{anime_id, episode_id, title, turnstile_token}` |
| POST | `/api/proxy/resolve-embed` | Returns `{proxied_url}` or 204; response key unknown (204 in testing) |
| GET | `/api/announcements/active` | Plain JSON |
| GET | `/api/fai-status` | Plain JSON |

Previous recon incorrectly identified the catalog endpoint as `/api/anime?page=N`
and suspected AES encryption. The actual encryption is simple repeating XOR.

## Encryption

**NOT AES** — despite high entropy (~7.70 bits/byte), analysis via
`JSON.parse` interception reveals simple repeating XOR:

- **Catalog** (`/api/catalog/search`): 4-byte key `[0xdf, 0x9f, 0x5f, 0x1f]`
- **Sources** (`POST /api/sources/anime`): 32-byte key, `key[j] = (0x51 + j) | (0x80 * (j & 1))`
  → `51 d2 53 d4 55 d6 57 d8 59 da 5b dc 5d de 5f e0 61 e2 63 e4 65 e6 67 e8 69 ea 6b ec 6d ee 6f f0`

Keys are static (session-independent, verified by curl with no cookies).

## Decrypted catalog schema (key fields)

```json
{
  "data": [
    {
      "id": "1326",
      "title": "L'Attaque des Titans",
      "slug": "l-attaque-des-titans",
      "poster_url": "https://image.tmdb.org/t/p/w500/...",
      "season_year": 2013,
      "status": "Ended",
      "genres": ["Animation", "..."],
      "languages": ["VF", "VOSTFR"]
    }
  ],
  "meta": { "total": 2797, "page": 1, "per_page": 32, "total_pages": 88 }
}
```

## Anime detail page schema (from inline `<script>` tag)

```json
{
  "anime": {
    "id": 1326,
    "title": { "romaji": "Shingeki no Kyojin", "userPreferred": "L'Attaque des Titans" },
    "description": "...",
    "coverImage": { "large": "https://image.tmdb.org/t/p/w500/..." },
    "genres": ["Animation", "..."],
    "status": "Ended",
    "episodesList": [
      { "id": 95409, "number": 1, "title": "...", "hasSources": true, "languages": ["VF", "VOSTFR"] }
    ]
  }
}
```

## Decrypted sources schema

```json
[
  { "id": 361656, "url": "https://vidmoly.biz/embed-3yw9j0gyz2a9.html", "host": "vidmoly", "language": "VOSTFR", "episodeId": 95409 },
  { "id": 361706, "url": "https://vidmoly.biz/embed-u1lzs6cp6r3k.html", "host": "vidmoly", "language": "VF",     "episodeId": 95409 }
]
```

Source URLs are embed pages (vidmoly.biz, sibnet.ru). mpv with yt-dlp can
resolve these to HLS streams.

## Resolve-embed

`POST /api/proxy/resolve-embed {sourceId, host, url}` returns `{proxied_url}`
on success or **204 No Content** when unavailable (server-side embed resolution
fails for most sources). When 204, fall back to the original embed URL.
The response encryption key is undetermined (endpoint consistently returns 204).

## Media-player integration

The player uses HLS.js with MSE. The `.m3u8` comes either from `proxied_url`
(when resolve-embed works) or from the embed page itself (vidmoly/sibnet),
which mpv can extract via yt-dlp (`--script-opts=ytdl_hook-ytdl_path=yt-dlp`).
