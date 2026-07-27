# Keyboard shortcuts

Defaults (see `input::default_binding`). Bindings are intended to be overridable
from `[keys]` in config.toml in a later phase.

## Global / browsing

| Key            | Action            |
|----------------|-------------------|
| `q` / `Ctrl-C` | Quit              |
| `j` / `↓`      | Move down         |
| `k` / `↑`      | Move up           |
| `h` / `←`      | Move left / back  |
| `l` / `→`      | Move right / into |
| `Enter`        | Select / drill in |
| `Esc`          | Back              |
| `/`            | Search            |
| `Tab`          | Switch tab (Home / Favourites / History) |
| `f`            | Toggle favourite  |
| `S`            | Cycle sort order (catalogue) |
| `F`            | Quick-filter loaded results (Enter keeps, Esc clears) |

Catalogue results paginate automatically: scrolling near the bottom loads the
next page (the header shows `shown/total · sort`). Sort (`S`) re-queries from the
server; the quick-filter (`F`) narrows the already-loaded results locally.

## Player

| Key       | Action                    |
|-----------|---------------------------|
| `Space`   | Play / pause              |
| `←` / `→` | Seek back / forward       |
| `+` / `-` | Volume up / down          |
| `m`       | Mute                      |
| `n` / `p` | Next / previous episode   |
| `s`       | Cycle / toggle subtitles  |
| `a`       | Cycle audio track         |
| `q`/`Esc` | Stop, return to details   |

## POC (`poc_kitty`)

`Space` pause · resize auto-realigns video · `q`/`Esc` quit.
