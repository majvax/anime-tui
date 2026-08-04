# Architecture

## Big picture

anime-tui is a keyboard-driven terminal client that browses/searches an anime
catalogue and plays episodes **inside a reserved rectangle of the terminal**
using the Kitty graphics protocol, with mpv as the decode/sync/audio/subtitle
engine.

The single most important environmental finding (see `POC_FINDINGS.md`) is that
**mpv ships a native `--vo=kitty` video output**. We therefore do NOT build a
frame pipeline (FFmpeg → RGBA → Kitty upload) ourselves. mpv decodes, keeps A/V
in sync, renders subtitles, plays audio, AND emits the Kitty graphics escapes —
confined to a cell rectangle we choose via `--vo-kitty-left/top/cols/rows`. We
control it over a JSON-IPC Unix socket. This is dramatically simpler and more
robust than a hand-rolled pipeline, and keeps us off "implement codecs" territory.

```
┌──────────────────────────────────────────────────────────────┐
│ app  (state machine, central async event loop)                │
│   ├── input   raw keys ─► typed Action (configurable, vim-ish) │
│   ├── ui      ratatui render; reserves + never overpaints the  │
│   │           video Rect                                       │
│   ├── provider  trait ─┬─ nakanime (isolated selectors/parse)  │
│   │                    └─ mock (fixtures, offline dev/tests)    │
│   ├── resolver  episode ─► validated playable URL (allowlist)  │
│   ├── player   ─┬─ mpv    spawn(arg array) + IPC socket        │
│   │             └─ kitty  capability probe, cell geometry,     │
│   │                        DELETE_ALL cleanup                   │
│   ├── database  SQLite: history / favourites / resume (atomic)  │
│   ├── cache     posters/metadata, sanitized keys               │
│   ├── config    TOML + platform paths                          │
│   ├── models    provider-agnostic domain types                 │
│   └── errors    structured Error/Result, no panics on IO paths  │
└──────────────────────────────────────────────────────────────┘
```

## Key design rules

- **Network off the render thread.** IO runs in Tokio tasks; results arrive as
  typed `AppEvent`s consumed by the central loop. `ui::render` is pure.
- **The video Rect is sacred.** In `View::Player`, `ui` draws chrome around the
  rectangle and nothing inside it, so TUI diff-redraws never corrupt mpv's image.
  `ui::video_rect` and `render_player` share one layout so they stay aligned on
  resize.
- **mpv is never shell-invoked.** `player::mpv::build_args` builds a `Vec<String>`
  argument array; the URL is placed after `--`. stdin is `/dev/null` so mpv can't
  fight the TUI for the keyboard.
- **One URL choke point.** `resolver::validate_stream_url` enforces an http/https
  scheme + host allowlist before any URL reaches mpv.
- **Provider isolation.** All Nakanime CSS selectors/URL templates live in
  `provider/nakanime/selectors.rs`; all parsing is pure (`parse.rs`, string in →
  models out) and fixture-tested. Layout drift surfaces as
  `Error::ProviderChanged { context, detail }`.
- **Cleanup on every exit path.** The player/POC own terminal + child mpv in a
  guard with `Drop` **and** a panic hook: kill mpv, emit `DELETE_ALL_IMAGES`,
  leave alt screen, disable raw mode, restore cursor.
- **Atomic persistence.** SQLite upserts for progress; no partial writes.
- **Secrets discipline.** Cookies/referer/user-agent/tokens are marked sensitive
  and never logged; the arg vector containing `--http-header-fields` is not logged
  verbatim.

## Backend selection

`player::select_backend` → `EmbeddedKitty` when `kitty::probe_support()` is true
(Kitty/Ghostty/WezTerm) unless `force_external_player` is set, else `ExternalMpv`
(standalone mpv) as the always-reliable fallback.

## Development phases

1. **Validation** — env probe + local-file Kitty POC (`examples/poc_kitty.rs`). ✅ done
2. **Skeleton** — async event loop (`app::run`), typed events/effects, config, DB,
   mock provider, external-mpv playback, live TUI views. ✅ done
3. **Provider** — Nakanime transport confirmed & wired (endpoints, cookies,
   retries, id validation, wiremock tests); **blocked at the encrypted-response
   boundary** — decryption intentionally not implemented (anti-scraping
   circumvention). Parsers are fixture-driven seams. See NAKANIME_RECON.md. ⚠️ partial
4. **Embedded playback** — `player::embedded` runs mpv `--vo=kitty` in the
   reserved rect, driven over IPC (pause/seek/volume/mute/sub/audio/next-prev),
   with `time-pos`/`duration`/`pause` observation, resume positions, resize
   respawn, and cleanup. External-mpv fallback retained. ✅ done
5. **Hardening** — caching, history, favourites, resume UI, docs, packaging.

### Embedded playback (Phase 4)

`EmbeddedPlayer::start` spawns mpv confined to `ui::video_rect(area)` with
`--start=+<resume>` and an IPC socket, then spawns an observer task that
`observe_property`s time-pos/duration/pause and forwards one `ProgressUpdate`
per whole second to the loop. Player-view keys map straight to IPC commands.
The loop saves progress every `progress_save_interval_secs` and on stop, detects
mpv exit on the tick, and on resize respawns mpv at the new rectangle resuming at
the last position (`current_source` is retained for exactly this).

**Known concurrency caveat (honest):** mpv writes Kitty graphics to the shared
stdout that ratatui also uses. We mitigate corruption by never drawing inside the
video rect and redrawing chrome at most ~1/sec during playback, but a fully
robust single-writer design (libmpv render API, or mpv on a dedicated tty) is the
proper long-term fix — see POC_FINDINGS.md.

### Phase 2 event flow

```
key ──► input::default_binding ──► Action
                                     │
                     App::on_action (pure) ──► Effect (Search/LoadDetails/Play)
                                     │
                app::run dispatch ──► tokio::spawn(provider/player IO)
                                     │
                          Msg (Results/Details/PlaybackEnded)
                                     │
                     App::set_* (pure) ──► ui::render (pure)
```

The loop (`app::run::Runner`) owns the `TerminalGuard`, provider (`Arc<dyn
Provider>`), and DB. `tokio::select!` merges terminal events, a 250ms tick, and
the background-result channel. Playback (`player::run_external`) runs in a task
and reports completion; history is written when it ends.
