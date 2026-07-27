# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Git

Commits in this repo must NOT include a Claude/AI `Co-Authored-By` trailer or any
"Generated with Claude" / "🤖" line. Author commits as the user only, with a plain
message. This overrides any global default that appends an AI co-author.

## What this is

Terminal (ratatui/crossterm) client for the Nakanime anime source that plays
episodes **embedded in the terminal** via the Kitty graphics protocol, using mpv
as the media engine. See `docs/ARCHITECTURE.md` and `docs/POC_FINDINGS.md` — read
those before non-trivial work; they explain the central design decision below.

## Commands

```bash
cargo build                          # library + both binaries
cargo test                           # all unit tests
cargo test <name>                    # single test, e.g. `cargo test progress_roundtrip`
cargo test -p anime-tui player::  # tests under one module path
cargo run --bin anime-tui            # app skeleton: prints backend + config path
cargo run --bin poc_kitty -- FILE    # playback proof-of-concept (needs a Kitty-graphics terminal)
scripts/gen_test_media.sh            # make /tmp/anime-tui-poc-test.mp4 for the POC
cargo clippy --all-targets           # lint
```

## The central architectural fact

**mpv on the target renders Kitty graphics itself (`--vo=kitty`).** We do NOT
build a decode→scale→upload frame pipeline. mpv handles decode, A/V sync,
subtitles, audio, and Kitty rendering, confined to a cell rectangle via
`--vo-kitty-left/top/cols/rows` with `--vo-kitty-alt-screen=no` and
`--vo-kitty-config-clear=no` so it paints into the TUI instead of taking over.
We drive mpv over a JSON-IPC Unix socket. If you find yourself writing an FFmpeg
frame loop, stop — that's the *contingency* path (`docs/POC_FINDINGS.md`), only
if a terminal lacks a working mpv kitty VO.

## Invariants — do not break these

- **`--vo-kitty-use-shm=yes` MUST stay in `player::mpv::build_args`.** Without
  SHM, frames are base64-encoded inline over the PTY: 24 fps × ~8 MB/frame
  ≈ 200 MB/s saturates the PTY, causing severe frame drops and making embedded
  playback unusable at HD resolutions. **Do not remove SHM to "fix" memory.**
- **Two distinct memory concerns, don't conflate them:**
  - **`/dev/shm` (kernel shm)** — mpv writes one SHM segment per frame; some
    terminals don't `shm_unlink` promptly, so segments pile up and can OOM the
    box. Bounded by the GC task in `EmbeddedPlayer` (pre-spawn snapshot + delete
    aged/new segments every 1 s, plus cleanup on stop/drop). This is the crash fix.
  - **Terminal image cache (e.g. Ghostty's multi-GB RSS)** — the terminal caches
    every transmitted RGBA frame and evicts by its own budget; it is NOT a leak
    and NOT freed by `/dev/shm` cleanup. It CANNOT be shrunk by rendering smaller:
    mpv's kitty VO always renders to the full cell box (`--vo-kitty-width/height`
    below the true terminal size just paints a shrunken image into the corner —
    the terminal does not upscale). The only real fix is the **external player**
    (`Runner::open_external`, key `o`; it is also the default backend), which uses a
    GPU window with no image cache. Users may also lower the terminal's own
    `image-storage-limit`. Do **not** add a periodic `DELETE_ALL_IMAGES` purge to
    "fix" it — that only causes visible flicker.
- **mpv tuning flows Config → Runner → `EmbeddedPlayer::start`/`run_external` →
  `build_args` as `MpvTuning`** (read-ahead caps only). `ui::video_rect`'s pixel
  dimensions MUST equal the real terminal size for the cell box.
- **Two playback backends; the external window is the DEFAULT.**
  `player::select_backend` returns `Backend::ExternalMpv` unless
  `config.embedded_player` is true AND `kitty::probe_support()` — i.e. embedded is
  strictly opt-in. *External window* (`Backend::ExternalMpv`; also reachable from
  embedded via the `o` key → `Runner::open_external`) is the **highest-quality**
  path: it uses `PreparedSource::original_url` (the validated provider URL) with
  `MpvTuning::high_quality()`, and `build_args` adds
  `--ytdl-format=bestvideo+bestaudio/best` for the External presentation so
  mpv+yt-dlp choose the best video+audio. *Embedded* (`Backend::EmbeddedKitty`)
  uses `PreparedSource::url` — a yt-dlp pre-resolved direct stream for fast first
  frame. Both URLs in `PreparedSource` are validated; keep it that way.

- **The video Rect is never overpainted.** In `View::Player`, `ui` draws chrome
  around the reserved rectangle and nothing inside it. `ui::video_rect` and
  `ui::render_player` MUST share one layout so video and chrome stay aligned on
  resize. Changing one without the other corrupts playback.
- **mpv is spawned with an argument array, never a shell**, via
  `player::mpv::build_args` (URL after `--`, stdin `/dev/null`). Don't format
  command strings.
- **Every URL passes `resolver::validate_stream_url`** (http/https + host
  allowlist) before reaching mpv.
- **Cleanup on every exit path**: kill mpv → emit `player::kitty::DELETE_ALL_IMAGES`
  → leave alt screen → disable raw mode. Implemented via a `Drop` guard AND a
  panic hook (see `src/bin/poc_kitty.rs`). Preserve both when refactoring.
- **Secrets discipline**: cookies, referer, user-agent, tokens, resolved stream
  URLs are sensitive. Never `tracing::*` them; never log the arg vector that
  contains `--http-header-fields`.
- **Persistence is atomic** (SQLite upserts in `database`).
- **Network stays off the render thread**: IO in Tokio tasks → typed `AppEvent`s
  → central loop. `ui::render` is pure.

## Provider rules (Nakanime) — READ BEFORE TOUCHING `provider/nakanime`

Transport is **real and confirmed** (host `nakanime.tv`; see
`docs/NAKANIME_RECON.md`): AdonisJS + Inertia SPA behind Cloudflare. Endpoints
live isolated in `provider/nakanime/endpoints.rs` (`/api/anime`,
`/api/anime/{id}`, `/anime/{id}`, `/episode/{id}`). The client uses a cookie
store + browser-like headers; ids are numeric and validated.

- Parsers in `parse.rs` take a **decrypted** `&str` and are pure/fixture-tested.
  they stop at a marked mapping point. Complete them only from a decrypted fixture
  the user legitimately provides — see `docs/PROVIDER_MAINTENANCE.md`.
- On wrong/blocked bodies, return `Error::ProviderChanged { context, detail }`
  (`errors::provider_changed(..)`).
- Mock HTTP with `wiremock` (`tests/nakanime_transport.rs`); never hit the live
  service in tests.

Use `provider::mock::MockProvider` to develop UI/playback offline (the app
defaults to it when `base_url` is empty).

## Module map

`app` state machine · `ui` render · `input` keys→`Action` · `provider`(+`nakanime`,
`mock`) · `resolver` URL validation · `player`(+`mpv`,`kitty`) · `database` SQLite
· `cache` · `config` · `models` · `errors`. Modules use `mod.rs` directories.

Static images (Kitty graphics, transmitted by us — video is still all mpv). Two
systems, both transmitted from the `Runner` run loop AFTER the draw so ratatui
doesn't overpaint them, both fed by a shared `reqwest::Client` and disk cache under
`Config::cache_dir()/posters`:
- **Details poster** (`transmit_png`, id `POSTER_ID`): high-res `extraLarge`, resized
  to the reserved box with Lanczos3. `ui::render` only *reserves* the column
  (`ui::details_poster_rect` must match `render_details`); `Runner::render_poster`
  paints it; `Runner::fetch_poster` → `Msg::Poster` off-thread.
- **Browse row thumbnails** (`transmit_png_id`, ids `THUMB_ID_BASE + slot`): a small
  low-res cover per visible list row. `render_list` reserves `THUMB_COLS`/`ROW_H` and
  renders at `App.list_offset` (kept by `Runner::update_list_offset` via `keep_in_view`
  so placement matches). `Runner::place_thumbnails` diffs/debounces and transmits;
  `fetch_thumb` → `Msg::Thumb` (bounded concurrency, in-memory `thumb_cache`).
Player controls route
through `Runner::player_command`, which works for BOTH backends (embedded handle or
the external window's IPC socket) — `i` skips the opening (`config.skip_intro_secs`).

## Conventions

- Errors: `errors::{Error, Result}` + `thiserror`. No `panic!`/`unwrap` on
  recoverable (IO/network/provider/player) paths; `unwrap` is fine in `#[cfg(test)]`.
- Keep pure logic (parsers, `build_args`, state transitions, geometry,
  sanitisation) free of IO so it stays unit-testable — that's why those functions
  take/return plain values. Follow that pattern when extending.

## Development phases (see ARCHITECTURE.md)

1 Validation ✅ (env probe + POC) · 2 Skeleton ✅ (`app::run` loop, typed
Action→Effect→Msg flow, config, DB, mock provider, external-mpv playback, live
views) · 3 Provider ← next (fixtures/tests) · 4 Embedded playback ✅
(`player::embedded`: mpv `--vo=kitty` in the reserved rect over IPC, controls,
progress observation, resume, resize respawn, cleanup; external fallback kept) ·
5 Hardening (cache, history, favourites, resume UI, packaging).

Embedded playback: `EmbeddedPlayer` (spawn + IPC + observer task) is started from
`Runner::begin_playback`; player-view keys map to IPC commands in
`Runner::on_player_key`; progress arrives as `ProgressUpdate` on a dedicated
channel. `ui::video_rect` MUST stay identical to `ui::render_player`'s video
surface or mpv paints over the progress bar. mpv shares stdout with ratatui —
keep redraws minimal during playback and never draw inside the video rect.

The event loop lives in `app::run::Runner`. Navigation is a pure
`App::on_action(Action) -> Effect`; the loop turns `Effect`s into `tokio::spawn`ed
provider/player IO that reports back as `Msg`, then calls pure `App::set_*` +
`ui::render`. Add features by extending those three enums, not by doing IO in
`App` or `ui`.
