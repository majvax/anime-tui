# Troubleshooting

### No video, but audio plays
The terminal likely lacks a working Kitty graphics VO. Confirm with
`mpv --vo=kitty <file>` directly. Use a Kitty-graphics terminal (Kitty, Ghostty,
WezTerm) or set `force_external_player = true` to use standalone mpv.

### `mpv: unknown option --vo-kitty-...`
mpv is older than the kitty VO placement options. Upgrade mpv (this project was
validated on mpv 0.41.0).

### Stale image left in the terminal after a crash
Every exit path emits `_Ga=d,d=A` (delete all placements). If a hard kill
skipped it, run `printf '\e_Ga=d,d=A\e\\'` or just clear the terminal. Report it
— cleanup should be guaranteed by the Drop guard + panic hook.

### Terminal stuck in raw mode after exit
Run `reset`. This shouldn't happen: raw mode is disabled in the guard and the
panic hook. If it does, capture how the process died.

### Video misaligned after resize
The POC respawns mpv aligned to the new rectangle. If misaligned, your terminal
may report cell size inconsistently mid-resize; try again after the resize settles.

### Provider returns "feature not implemented"
Expected until Phase 3. Nakanime endpoints/selectors are deliberately unverified
placeholders — see `docs/PROVIDER_MAINTENANCE.md`.

### An episode source won't play (mpv exits immediately)
Playback resolves embed hosts with **yt-dlp**, so a host only works if yt-dlp can
extract it. The app tries the episode's sources best-first and **falls back**
automatically when one fails (you'll see "trying source …"), so a dead host yields
to a working one.
- **sibnet / vidmoly**: supported (sibnet has a yt-dlp extractor; vidmoly resolves
  via yt-dlp's generic extractor). If they fail, run `yt-dlp -U` to update.
- **voe**: current yt-dlp has *no* voe extractor, so voe can't be resolved — the
  app falls back to another source. If an episode's *only* source is voe, it can't
  play until yt-dlp adds voe support (or a native resolver is contributed from a
  real voe embed sample).
Diagnose a specific host with:
`yt-dlp -f best --print "%(url)s" --print "%(http_headers)j" '<embed URL>'`.

### High CPU during playback
`--vo=kitty` uploads frames as escape sequences; cost scales with the video
rectangle's pixel area and fps. Shrink the reserved rectangle, or try
`--vo-kitty-use-shm` (shared memory). Record numbers in `docs/POC_FINDINGS.md`.
