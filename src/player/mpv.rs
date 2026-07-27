//! mpv control via a JSON-IPC Unix socket.
//!
//! mpv is ALWAYS spawned with a structured argument array (never a shell), with
//! stdin redirected from /dev/null so it cannot fight the TUI for the keyboard.
//! Control (pause, seek, volume, track selection, property observation) goes
//! over the IPC socket rather than fragile keypress injection.
//!
//! Two backends share this launcher:
//!   * External:  standalone/windowed mpv (reliable fallback, Phase 2).
//!   * Embedded:  `--vo=kitty` painted into a reserved cell rectangle (Phase 4).

use super::kitty::CellRect;
use crate::errors::{Error, Result};

/// How mpv should present video.
#[derive(Debug, Clone)]
pub enum Presentation {
    /// Standalone mpv window / default VO. `input_conf` is an optional path to an
    /// mpv `input.conf` that mirrors the TUI keybinds so the WINDOW responds to the
    /// same keys (mpv's default bindings/mouse are kept too).
    External { input_conf: Option<String> },
    /// Embedded Kitty output confined to `rect` cells, painting into our screen
    /// (alt-screen and terminal-clear disabled so it doesn't hijack the TUI).
    EmbeddedKitty { rect: CellRect },
}

/// Build an mpv `input.conf` that mirrors the TUI player keybinds, so the external
/// window responds to the same keys when it's focused. `q`/`ESC` quit (returning to
/// the TUI). Episode nav (n/p) is intentionally omitted — it needs the TUI.
pub fn external_input_conf(skip_intro_secs: u64) -> String {
    format!(
        "# anime-tui: keep in sync with Runner::on_player_key\n\
         SPACE cycle pause\n\
         h seek -10\n\
         LEFT seek -10\n\
         l seek 10\n\
         RIGHT seek 10\n\
         , seek -5\n\
         . seek 5\n\
         i seek {skip_intro_secs}\n\
         UP add volume 5\n\
         = add volume 5\n\
         + add volume 5\n\
         DOWN add volume -5\n\
         - add volume -5\n\
         m cycle mute\n\
         s cycle sub\n\
         a cycle aid\n\
         f cycle fullscreen\n\
         q quit\n\
         ESC quit\n"
    )
}

/// Bounds on mpv's own read-ahead buffer so it doesn't hold large amounts of the
/// stream in RAM ("charging ahead"). Independent of the terminal image cache.
#[derive(Debug, Clone, Copy)]
pub struct MpvTuning {
    /// Max forward demuxer cache, MiB (`--demuxer-max-bytes`).
    pub max_buffer_mib: u64,
    /// Seconds to read ahead (`--demuxer-readahead-secs` / `--cache-secs`).
    pub readahead_secs: u64,
}

impl Default for MpvTuning {
    fn default() -> Self {
        Self { max_buffer_mib: 64, readahead_secs: 10 }
    }
}

impl MpvTuning {
    /// Generous buffer for the standalone-window backend, which has no terminal
    /// image cache to worry about — favour smooth high-bitrate/HD playback.
    pub fn high_quality() -> Self {
        Self { max_buffer_mib: 256, readahead_secs: 30 }
    }
}

/// Build the mpv argument vector. Pure + no IO so it is unit-testable, and so
/// the exact args handed to the process are auditable in one place.
///
/// `headers` (referer/user-agent/cookies) are passed via `--http-header-fields`
/// and are sensitive: callers must not log the returned vector verbatim.
pub fn build_args(
    url: &str,
    ipc_socket: &str,
    presentation: &Presentation,
    headers: &[(String, String)],
    tuning: &MpvTuning,
) -> Vec<String> {
    let mut args = vec![
        "--no-config".into(),
        "--really-quiet".into(),
        "--input-terminal=no".into(), // do not read keys from the shared tty
        format!("--input-ipc-server={ipc_socket}"),
        "--keep-open=no".into(),
        "--idle=no".into(),
    ];

    // Bound mpv's own read-ahead so a long stream doesn't sit in RAM. Applies to
    // both backends. The back-buffer is half the forward cap. These are mpv's
    // memory, distinct from the terminal's Kitty image cache.
    args.push(format!("--demuxer-max-bytes={}MiB", tuning.max_buffer_mib));
    args.push(format!(
        "--demuxer-max-back-bytes={}MiB",
        (tuning.max_buffer_mib / 2).max(1)
    ));
    args.push(format!("--demuxer-readahead-secs={}", tuning.readahead_secs));
    args.push(format!("--cache-secs={}", tuning.readahead_secs));

    // Start playing as soon as any data is available instead of waiting to fill
    // the cache — reduces time-to-first-frame on network streams (both backends).
    args.push("--cache-pause=no".into());

    match presentation {
        Presentation::External { input_conf } => {
            // Standalone window. If mpv is handed a page URL (yt-dlp pre-resolution
            // failed) let its ytdl hook pick the best stream; when it already gets
            // a direct URL — the fast path — this is a harmless no-op.
            args.push("--ytdl-format=bestvideo+bestaudio/best".into());
            // The window keeps mpv's default bindings (mouse, OSC seek bar); our
            // conf overrides the specific keys to match the TUI.
            if let Some(path) = input_conf {
                args.push(format!("--input-conf={path}"));
            }
        }
        Presentation::EmbeddedKitty { rect } => {
            // Embedded shares the terminal and is driven purely over IPC, so it must
            // have no key bindings of its own.
            args.push("--no-input-default-bindings".into());
            args.push("--vo=kitty".into());
            args.push("--vo-kitty-alt-screen=no".into());
            args.push("--vo-kitty-config-clear=no".into());
            // Preserve video aspect ratio; letterbox with black bars rather
            // than stretching to fill the cell rectangle.
            args.push("--keepaspect=yes".into());
            args.push(format!("--vo-kitty-left={}", rect.left));
            args.push(format!("--vo-kitty-top={}", rect.top));
            args.push(format!("--vo-kitty-cols={}", rect.cols));
            args.push(format!("--vo-kitty-rows={}", rect.rows));
            if let (Some(w), Some(h)) = (rect.pixel_width, rect.pixel_height) {
                // Telling mpv the exact pixel dimensions it can paint avoids it
                // having to query the terminal itself and removes upscale/downscale
                // rounding artefacts at the cell boundary.
                args.push(format!("--vo-kitty-width={w}"));
                args.push(format!("--vo-kitty-height={h}"));
            }
            // SHM bypasses PTY base64 encoding entirely, which is required for
            // smooth playback at HD resolutions. Without it, 24 fps × ~8 MB/frame
            // ≈ 200 MB/s saturates the PTY and causes severe frame drops.
            // Memory growth from un-unlinked SHM segments is mitigated by:
            //   1. EmbeddedPlayer::stop / Drop — purge /dev/shm entries on exit.
            //   2. Runner::on_tick periodic DELETE_ALL_IMAGES — flushes Ghostty's
            //      image cache every 30 s so decoded frame data is freed.
            // DO NOT remove this flag — base64 fallback is unusable at HD.
            args.push("--vo-kitty-use-shm=yes".into());
            // Decouple frame delivery from display-sync timing. Without this mpv
            // tries to hit an exact display refresh rate that the terminal cannot
            // signal back, causing dropped/stuttered frames.
            args.push("--video-sync=display-desync".into());
            // Hardware decoding reduces per-frame CPU load.
            args.push("--hwdec=auto".into());
        }
    }

    // `--http-header-fields` is a COMMA-separated list with no escaping, so any
    // header whose value contains a comma (e.g. an `Accept`/`Accept-Language`
    // captured from yt-dlp) would corrupt the whole list and break every request.
    // Drop those defensively — callers should already allowlist safe headers.
    let safe: Vec<&(String, String)> = headers.iter().filter(|(_, v)| !v.contains(',')).collect();
    if !safe.is_empty() {
        let joined = safe
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(",");
        args.push(format!("--http-header-fields={joined}"));
    }

    args.push("--".into());
    args.push(url.to_string());
    args
}

/// A single JSON-IPC command line (newline-terminated) for mpv's socket.
pub fn ipc_command(command: &[serde_json::Value]) -> Result<String> {
    let payload = serde_json::json!({ "command": command });
    let mut line = serde_json::to_string(&payload).map_err(|e| Error::Player(e.to_string()))?;
    line.push('\n');
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_args_have_no_kitty_vo() {
        let a = build_args(
            "https://x/v.m3u8",
            "/tmp/s",
            &Presentation::External { input_conf: None },
            &[],
            &MpvTuning::default(),
        );
        assert!(!a.iter().any(|s| s.contains("vo=kitty")));
        assert!(a.iter().any(|s| s == "--input-terminal=no"));
        // URL is last and separated by `--` so it can't be read as a flag.
        assert_eq!(a.last().unwrap(), "https://x/v.m3u8");
        assert_eq!(a[a.len() - 2], "--");
    }

    #[test]
    fn external_window_gets_our_bindings_embedded_does_not() {
        // External: no default-bindings suppression, and our conf when provided.
        let ext = build_args(
            "https://x/v",
            "/tmp/s",
            &Presentation::External { input_conf: Some("/tmp/anime-tui-input.conf".into()) },
            &[],
            &MpvTuning::default(),
        );
        assert!(!ext.iter().any(|s| s == "--no-input-default-bindings"));
        assert!(ext.iter().any(|s| s == "--input-conf=/tmp/anime-tui-input.conf"));
        // Embedded: bindings suppressed (driven over IPC), never an input-conf.
        let rect = CellRect { left: 0, top: 0, cols: 8, rows: 8, pixel_width: None, pixel_height: None };
        let emb = build_args(
            "https://x/v",
            "/tmp/s",
            &Presentation::EmbeddedKitty { rect },
            &[],
            &MpvTuning::default(),
        );
        assert!(emb.iter().any(|s| s == "--no-input-default-bindings"));
        assert!(!emb.iter().any(|s| s.starts_with("--input-conf")));
    }

    #[test]
    fn external_input_conf_mirrors_tui_binds() {
        let conf = external_input_conf(85);
        assert!(conf.contains("SPACE cycle pause"));
        assert!(conf.contains("l seek 10"));
        assert!(conf.contains("h seek -10"));
        assert!(conf.contains("i seek 85"));
        assert!(conf.contains("q quit"));
    }

    #[test]
    fn embedded_args_carry_placement() {
        let rect = CellRect { left: 3, top: 2, cols: 40, rows: 20, pixel_width: None, pixel_height: None };
        let a = build_args(
            "https://x/v",
            "/tmp/s",
            &Presentation::EmbeddedKitty { rect },
            &[],
            &MpvTuning::default(),
        );
        assert!(a.iter().any(|s| s == "--vo=kitty"));
        assert!(a.iter().any(|s| s == "--vo-kitty-cols=40"));
        assert!(a.iter().any(|s| s == "--vo-kitty-left=3"));
        assert!(a.iter().any(|s| s == "--vo-kitty-alt-screen=no"));
    }

    #[test]
    fn external_requests_best_quality() {
        let a = build_args(
            "https://x/v",
            "/tmp/s",
            &Presentation::External { input_conf: None },
            &[],
            &MpvTuning::high_quality(),
        );
        assert!(a.iter().any(|s| s == "--ytdl-format=bestvideo+bestaudio/best"));
        assert!(a.iter().any(|s| s == "--demuxer-max-bytes=256MiB"));
        // Best-quality format selection is external-only, never embedded.
        let rect = CellRect { left: 0, top: 0, cols: 10, rows: 10, pixel_width: None, pixel_height: None };
        let e = build_args(
            "https://x/v",
            "/tmp/s",
            &Presentation::EmbeddedKitty { rect },
            &[],
            &MpvTuning::default(),
        );
        assert!(!e.iter().any(|s| s.starts_with("--ytdl-format")));
    }

    #[test]
    fn tuning_bounds_readahead_buffer() {
        let t = MpvTuning { max_buffer_mib: 64, readahead_secs: 8 };
        let a = build_args("https://x/v", "/tmp/s", &Presentation::External { input_conf: None }, &[], &t);
        assert!(a.iter().any(|s| s == "--demuxer-max-bytes=64MiB"));
        assert!(a.iter().any(|s| s == "--demuxer-max-back-bytes=32MiB"));
        assert!(a.iter().any(|s| s == "--cache-secs=8"));
    }

    #[test]
    fn headers_are_joined() {
        let h = vec![("Referer".into(), "https://ref".into())];
        let a = build_args(
            "https://x/v",
            "/tmp/s",
            &Presentation::External { input_conf: None },
            &h,
            &MpvTuning::default(),
        );
        assert!(a.iter().any(|s| s == "--http-header-fields=Referer: https://ref"));
    }

    #[test]
    fn comma_valued_headers_are_dropped() {
        // A comma in a value would corrupt the comma-separated field list.
        let h = vec![
            ("Referer".into(), "https://ref".into()),
            ("Accept".into(), "text/html,application/xml".into()),
        ];
        let a = build_args(
            "https://x/v",
            "/tmp/s",
            &Presentation::External { input_conf: None },
            &h,
            &MpvTuning::default(),
        );
        // Only the comma-free header survives, and it's the whole field value.
        assert!(a.iter().any(|s| s == "--http-header-fields=Referer: https://ref"));
        assert!(!a.iter().any(|s| s.contains("Accept")));
    }

    #[test]
    fn ipc_command_is_newline_terminated_json() {
        let line = ipc_command(&[serde_json::json!("cycle"), serde_json::json!("pause")]).unwrap();
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"command\""));
    }
}
