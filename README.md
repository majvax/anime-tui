# anime-tui

A keyboard-driven terminal client for browsing, searching, and playing anime —
with video rendered **inside the terminal** (Kitty graphics protocol) and mpv as
the decode/audio/subtitle/sync engine. Inspired by moviebox-tui.

> Status: **early.** Phases 1–2 (env validation + POC; async app skeleton with
> live TUI views and external-mpv playback) and Phase 4 (embedded Kitty playback
> driven over mpv IPC — pause/seek/volume/mute/subtitle/audio/next-prev, live
> progress, resume, resize) are in place. The Nakanime provider (Phase 3) is an
> interface with fixtures; its endpoints/selectors are intentionally unverified
> (see below), so playback currently runs against the offline mock catalogue.

## System requirements

- Rust 1.85+ (validated on 1.95)
- `mpv` **0.41+** (with the `kitty` video output — `mpv --vo=help | grep kitty` —
  if you want the opt-in embedded backend)
- `yt-dlp` on `PATH` (used to pick the best stream for the default window backend)
- By default playback opens in a standalone mpv **window** — no special terminal
  needed. The opt-in embedded backend additionally needs a Kitty-graphics terminal
  (**Kitty, Ghostty, or WezTerm**); on other terminals it falls back to the window.
- `ffmpeg` (only to generate the POC test clip; libav* also enables the future
  fallback frame pipeline)
- SQLite is bundled via `rusqlite` — no system package needed.

Arch Linux: `sudo pacman -S mpv ffmpeg rust`

## Build

```bash
cargo build            # library + binaries
cargo test             # unit tests (parsers, URL validation, state, persistence)
```

## Install

Runtime dependencies for all methods: **`mpv`** and **`yt-dlp`** on `PATH`
(`sudo pacman -S mpv yt-dlp`).

### Arch Linux (AUR)

```bash
yay -S anime-tui-bin     # prebuilt static binary — no compile
yay -S anime-tui         # build from source
```

`anime-tui-bin` and `anime-tui` both provide the `anime-tui` binary; pick one.

### Prebuilt binary (any x86_64 Linux)

Download the static `x86_64-unknown-linux-musl` tarball from the
[Releases](https://github.com/majvax/anime-tui/releases) page, verify, and drop it
on your `PATH`:

```bash
tar xzf anime-tui-x86_64-unknown-linux-musl.tar.gz
install -Dm755 anime-tui ~/.local/bin/anime-tui
```

### From source with Cargo

Install the `anime-tui` binary into your Cargo bin directory (`~/.cargo/bin`,
which should be on your `PATH`):

```bash
cargo install --path . --bin anime-tui     # from a checkout of this repo
```

Then run it from anywhere:

```bash
anime-tui
```

To update after pulling changes, re-run the same `cargo install` command (add
`--force` if Cargo says the binary is already installed). To uninstall:
`cargo uninstall anime-tui` (the package name), which removes `anime-tui`.

> Maintainers: see `docs/RELEASING.md` for how tags become Releases and AUR updates.

> Config, history and cache live in the platform dirs for `anime-tui`
> (e.g. `~/.config/anime-tui/config.toml` on Linux) — the running app prints the
> exact config path on startup.

## Try the playback proof-of-concept

Validates the whole embedded-playback pipeline on a local file (no network):

```bash
scripts/gen_test_media.sh                          # -> /tmp/anime-tui-poc-test.mp4
cargo run --example poc_kitty -- /tmp/anime-tui-poc-test.mp4
```

`Space` pauses (over mpv IPC), resizing realigns the video, `q` quits and
restores the terminal. Run it in Kitty/Ghostty/WezTerm — it can't render in a
plain terminal.

## Run the app

```bash
cargo run --bin anime-tui          # interactive TUI (uses the mock provider until base_url is set)
```

With no `base_url` configured it runs against an offline mock catalogue so you
can exercise search → details → episodes → playback. `/` to search, `j/k` to
move, `Enter` to drill in, `q` to quit. The results list **paginates as you
scroll** (the header shows `shown/total · sort`); `S` cycles the sort order
(re-queried from the server) and `F` opens a **quick-filter** that narrows the
loaded results locally (`Enter` keeps it, `Esc` clears). `Enter` on an episode
plays the **default source** directly (`default_source`, e.g. `vidmoly (VF)`) —
skipping the picker — and if that host fails it **falls back** to the next
available source automatically; press `c` to open the picker and choose a source
yourself. Playback launches mpv in a standalone window by default; set
`embedded_player = true` for in-terminal (Kitty) playback.

On a Kitty-graphics terminal (Kitty/Ghostty/WezTerm) the details page shows the
anime's **cover art** (fetched once and cached under the cache dir). During
playback, `i` **skips the opening** (`skip_intro_secs`, default 85 s). Player
controls (`Space`, `h`/`l`, `,`/`.`, `i`, volume, …) work from the terminal for
both backends over IPC, and — for the standalone window — the **same keys work
when the mpv window itself is focused** (via a generated `input.conf`). Episode
next/prev (`n`/`p`) are terminal-only.

## Configuration

Copy `config.example.toml` to the platform config dir (printed by the app) as
`config.toml`. `base_url` is empty by default — set it to the Nakanime host you
are authorized to access. See `docs/KEYBINDINGS.md` for shortcuts.

CLI flags: `anime-tui --paths` prints the config/data/cache directories,
`--config <path>` uses an alternate config file, and `--version` / `--help` do
the obvious.

## Backends & performance

Playback has two backends. The **default is a standalone mpv window** — it renders
to a GPU surface, has no per-frame memory overhead, and is the highest-quality
path: it plays the original stream and lets mpv+yt-dlp pick the best video+audio
(`--ytdl-format=bestvideo+bestaudio/best`) with a generous buffer.

The **opt-in embedded backend** (`embedded_player = true`, Kitty-graphics terminal
only) plays video *inside* the terminal, but has mpv send **every frame as an
uncompressed RGBA bitmap** over the Kitty protocol (≈8 MB/frame at 1080p). The
terminal caches those images, so its memory sits in the multi-GB range — this is
the terminal's image cache, not a leak. (mpv's kitty VO always renders to the full
cell box and the terminal does not upscale, so reducing resolution can't shrink
that memory without shrinking the picture into a corner — which is why there's no
quality slider; use the window backend instead.)

If you're on the embedded backend and want out:

- **On the fly:** press `o` to hand the current episode (at the current position)
  to a normal mpv window at best quality; the TUI returns to the episode list.
- **Permanently:** set `embedded_player = false` (the default).

If you stay on embedded, you can hard-limit the terminal's own image cache — in
Ghostty's config: `image-storage-limit = 536870912` (512 MB) — but keep it well
above one frame or the terminal may evict the frame being displayed and flicker.

`mpv`'s own read-ahead buffer is capped separately via `[playback] max_buffer_mib`
and `readahead_secs` so a long stream doesn't accumulate in RAM.

For truly borderless fullscreen in Ghostty, add to its config:

```
window-decoration = false
window-padding-x = 0
window-padding-y = 0
```

and toggle in-app fullscreen with `f`.

## Documentation

- `docs/ARCHITECTURE.md` — modules and design rules
- `docs/POC_FINDINGS.md` — environment validation + the mpv/Kitty finding
- `docs/PROVIDER_MAINTENANCE.md` — how to fill in / repair the Nakanime provider
- `docs/KEYBINDINGS.md`, `docs/TROUBLESHOOTING.md`

## Scope & ethics

Only content you are authorized to access. No DRM/auth/paywall bypass. Cookies,
tokens, referer/user-agent headers and stream URLs are treated as sensitive and
never logged.
