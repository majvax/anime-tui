# Phase 1 — Environment validation & POC findings

Measured on the development machine, 2026-07-25.

## Toolchain / dependency versions

| Component      | Version           | Notes                                    |
|----------------|-------------------|------------------------------------------|
| rustc / cargo  | 1.95.0            | edition 2021, MSRV pinned 1.85           |
| mpv            | 0.41.0            | **has `--vo=kitty`** and `--input-ipc-server` |
| libmpv         | 2.5.0 (pkg-config)| available if we ever want in-proc embed  |
| ffmpeg         | n8.1.1            | full libav* present (fallback pipeline)  |
| sqlite3        | 3.53.1            | + rusqlite `bundled` regardless          |
| yt-dlp         | 2026.03.17        | candidate for source resolution          |
| terminal       | Ghostty           | `TERM_PROGRAM=ghostty`, `TERM=xterm-ghostty` |

## Critical finding: mpv renders Kitty graphics natively

The spec warned: *"Do not assume mpv can directly output video through Kitty
graphics."* On this machine it **can**:

```
$ mpv --vo=help | grep kitty
  kitty   Kitty terminal graphics protocol
```

and it exposes cell-based placement, which is exactly what an embedded reserved
rectangle needs:

```
--vo-kitty-left / --vo-kitty-top     offset in terminal cells
--vo-kitty-cols / --vo-kitty-rows    size in terminal cells
--vo-kitty-alt-screen=no             render into the CURRENT screen (our TUI)
--vo-kitty-config-clear=no           don't clear the terminal
--vo-kitty-use-shm                   shared-memory transfer (perf lever)
```

**Consequence for the design:** we spawn mpv, confine `--vo=kitty` to the
ratatui `Rect`, disable its alt-screen/clear, and drive it over IPC. No custom
decode/scale/upload loop. Aspect ratio, A/V sync, subtitles and audio are mpv's
job. This is the primary architecture; the FFmpeg-frame-pipeline is only a
contingency if a target terminal lacks a working mpv kitty VO.

## Terminal note

Terminal is **Ghostty**, not Kitty. Ghostty implements the Kitty graphics
protocol, so embedded playback works, but there is **no `kitty`/`kitten`
binary** — do not depend on `kitten icat`. Capability detection keys off
`KITTY_WINDOW_ID` / `TERM` / `TERM_PROGRAM` (`player::kitty::probe_support`).

## The POC (`src/bin/poc_kitty.rs`)

Reserves a rectangle, spawns `mpv --vo=kitty` into it with IPC, toggles pause
over the socket, respawns aligned on resize, and on every exit path kills mpv +
emits `_Ga=d,d=A` (delete all placements) + restores the terminal.

### How to run / measure (must be a Kitty-graphics terminal)

```
scripts/gen_test_media.sh                 # writes /tmp/anime-tui-poc-test.mp4
cargo run --bin poc_kitty -- /tmp/anime-tui-poc-test.mp4
```

### Non-interactive validation performed here

A headless/graphics terminal isn't available to this build shell, so visual
confirmation is a manual step for the developer. What WAS verified automatically:

- `mpv --vo=kitty` accepts the placement args and emits Kitty graphics APC
  sequences (`ESC _ G …`) when driven through a pty — see the build/test log.
- The project + POC compile; unit tests for arg-building, URL validation,
  filename sanitisation, DB upsert and state transitions pass.

### Phase 4 IPC contract — validated headlessly

Real mpv (0.41) over its JSON-IPC socket emits exactly what
`player::embedded::observe` parses:

```
property-change time-pos  data=0.8   (float)
property-change duration  data=30.0  (float)
property-change pause     data=True  (bool)
```

and accepts our command shape (`{"command":["cycle","pause"]}` flipped pause).
So the embedded control/observation path is validated without a graphics
terminal; only the visual frame rendering still needs manual confirmation in
Kitty/Ghostty/WezTerm.

### Open design question: the shared-stdout writer

mpv `--vo=kitty` and ratatui both write to stdout. Current mitigation: never draw
inside the video rect + throttle chrome redraws to ~1/sec. If frame corruption
shows up on real hardware, move to a single-writer design (libmpv render API, or
mpv on a dedicated pty). Decide after measuring.

### To be measured on real hardware (fill in)

- [ ] CPU usage at 720p / 1080p with `--vo=kitty` (and with `--vo-kitty-use-shm`)
- [ ] Effective frame rate / dropped frames (`mpv` stats)
- [ ] Input→action latency over the IPC socket
- [ ] Resize behaviour: flicker, stale placements, realignment correctness
- [ ] Whether repositioning is better via respawn (current POC) vs. a live
      `vo` reconfigure — the POC respawns for simplicity.
