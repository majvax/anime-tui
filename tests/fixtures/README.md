# Test fixtures

Captured provider responses used by parser unit tests. **Populate from
authorized access only** (see `docs/PROVIDER_MAINTENANCE.md`). Suggested files:

- `search_<query>.html`   — a search results page
- `details_<id>.html`     — an anime detail page (metadata + episode list)
- `embed_<id>.html`       — an episode/embed page used for source resolution
- `source_<id>.json`      — the resolved-source response

Tests must read these files; they must never hit the live service.
