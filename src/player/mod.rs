//! Playback abstraction. Backends control mpv over IPC; presentation is either
//! embedded Kitty output or an external mpv process (fallback).

pub mod embedded;
pub mod kitty;
pub mod mpv;

use tokio::sync::mpsc;

use crate::config::Config;
use crate::errors::{Error, Result};
use crate::player::embedded::{observe, ProgressUpdate};
use crate::player::kitty::CellRect;
use crate::player::mpv::{build_args, MpvTuning, Presentation};

/// IPC socket path for the external-window backend. Deterministic per app process
/// (only one external play runs at a time) so the event loop can send it `quit`.
pub fn external_socket_path() -> String {
    format!("/tmp/anime-tui-mpv-{}.sock", std::process::id())
}

/// Phase 2 reliable backend: play `url` in an external/standalone mpv window and
/// await its exit. mpv is spawned with a structured argument array (never a
/// shell) and stdin from /dev/null so it can't fight the TUI for the keyboard.
/// `headers` (referer/user-agent/cookies) are sensitive — never log the args.
///
/// Progress is observed over the same IPC socket the embedded backend uses and
/// forwarded on `progress_tx`, so the window backend records resume positions
/// exactly like embedded playback (including after an early quit).
pub async fn run_external(
    mpv_path: &str,
    url: &str,
    headers: &[(String, String)],
    start_pos: f64,
    tuning: MpvTuning,
    input_conf: Option<String>,
    progress_tx: mpsc::UnboundedSender<ProgressUpdate>,
) -> Result<()> {
    // IPC socket for control/observe (only one external play at a time).
    let socket = external_socket_path();
    let _ = std::fs::remove_file(&socket);
    let mut args = build_args(
        url,
        &socket,
        &Presentation::External { input_conf },
        headers,
        &tuning,
    );
    if start_pos > 1.0 {
        let sep = args.iter().position(|a| a == "--").unwrap_or(args.len());
        args.insert(sep, format!("--start=+{start_pos:.3}"));
    }

    let mut child = tokio::process::Command::new(mpv_path)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::Player(format!("failed to launch mpv: {e}")))?;

    // Observe time-pos/duration over IPC until mpv exits and the socket closes.
    let observer = tokio::spawn(observe(socket.clone(), progress_tx));

    let status = child
        .wait()
        .await
        .map_err(|e| Error::Player(format!("mpv wait failed: {e}")))?;

    observer.abort();
    let _ = std::fs::remove_file(&socket);
    if status.success() {
        Ok(())
    } else {
        Err(Error::Player(format!("mpv exited with {status}")))
    }
}

/// Chosen playback backend for this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    EmbeddedKitty,
    ExternalMpv,
}

/// Decide the backend. The default is the standalone mpv window (higher quality,
/// no terminal image-cache RAM). Embedded Kitty playback is opt-in and only used
/// when the config asks for it AND the terminal actually supports Kitty graphics.
pub fn select_backend(config: &Config) -> Backend {
    if config.embedded_player && kitty::probe_support() {
        Backend::EmbeddedKitty
    } else {
        Backend::ExternalMpv
    }
}

impl Backend {
    pub fn presentation(self, video_rect: CellRect) -> Presentation {
        match self {
            Backend::EmbeddedKitty => Presentation::EmbeddedKitty { rect: video_rect },
            Backend::ExternalMpv => Presentation::External { input_conf: None },
        }
    }
}
