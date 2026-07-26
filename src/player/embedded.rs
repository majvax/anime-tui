//! Embedded playback: mpv with `--vo=kitty` confined to a reserved cell
//! rectangle, controlled over its JSON-IPC socket. mpv decodes, syncs A/V,
//! renders subtitles and Kitty frames; we position it, drive controls, and
//! observe `time-pos`/`duration`/`pause`.
//!
//! Concurrency note: mpv writes Kitty graphics to the shared stdout while the
//! TUI also writes there. We mitigate corruption by (a) never drawing inside the
//! video rectangle and (b) redrawing chrome at most ~1/sec during playback. A
//! fully robust single-writer design (libmpv render API, or mpv on a dedicated
//! tty) is noted as future work in docs/POC_FINDINGS.md.

use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::errors::{Error, Result};
use crate::player::kitty::CellRect;
use crate::player::mpv::{build_args, ipc_command, MpvTuning, Presentation};

/// One throttled playback-position sample forwarded to the event loop (~1/sec).
#[derive(Debug, Clone, Copy)]
pub struct ProgressUpdate {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    /// Rolling fps measured from mpv property-change events (~30-frame window).
    pub fps: Option<f64>,
}

/// A running embedded mpv instance plus its control socket.
pub struct EmbeddedPlayer {
    child: tokio::process::Child,
    socket: String,
    /// /dev/shm entries that existed before this mpv was spawned. Used to
    /// identify and remove SHM segments mpv leaves behind on stop.
    shm_before: HashSet<String>,
    /// Background GC task that removes aged SHM objects every few seconds.
    /// Aborting it (via drop or stop) is idempotent.
    gc_task: tokio::task::JoinHandle<()>,
}

impl EmbeddedPlayer {
    /// Spawn mpv confined to `rect`, resuming at `start_pos` seconds, and start a
    /// background task that forwards throttled progress on `progress_tx`.
    pub async fn start(
        mpv_path: &str,
        url: &str,
        headers: &[(String, String)],
        rect: CellRect,
        start_pos: f64,
        tuning: MpvTuning,
        progress_tx: mpsc::UnboundedSender<ProgressUpdate>,
    ) -> Result<Self> {
        let socket = format!("/tmp/anime-tui-embed-{}.sock", std::process::id());
        let _ = std::fs::remove_file(&socket);

        // Snapshot /dev/shm before spawning so we can clean up mpv's SHM
        // objects after stop. Ghostty does not always call shm_unlink after
        // reading each frame, leaving segments that accumulate across minutes.
        let shm_before = shm_snapshot();

        let mut args =
            build_args(url, &socket, &Presentation::EmbeddedKitty { rect }, headers, &tuning);
        if start_pos > 1.0 {
            // Insert before the `--` separator so it's parsed as an option.
            let sep = args.iter().position(|a| a == "--").unwrap_or(args.len());
            args.insert(sep, format!("--start=+{start_pos:.3}"));
        }

        let child = tokio::process::Command::new(mpv_path)
            .args(&args)
            .stdin(Stdio::null()) // don't fight the TUI for the keyboard
            .stdout(Stdio::inherit()) // Kitty graphics escapes reach our terminal
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::Player(format!("failed to launch mpv: {e}")))?;

        let sock = socket.clone();
        tokio::spawn(async move { observe(sock, progress_tx).await });

        // Periodic GC: remove SHM segments Ghostty left un-unlinked. This frees
        // KERNEL shm (what caused the earlier OOM crash) — not Ghostty's own
        // image cache, which is bounded by terminal eviction / the external player.
        // Frames arrive ~40 ms apart, so a 1 s age threshold guarantees the
        // terminal has already consumed anything we delete; running every 1 s
        // keeps /dev/shm near-empty ("delete played frames" promptly).
        let gc_before = shm_before.clone();
        let gc_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                shm_cleanup_aged(&gc_before, Duration::from_secs(1));
            }
        });

        Ok(Self { child, socket, shm_before, gc_task })
    }

    /// Send a raw mpv IPC command (best effort — ignored if the socket is gone).
    pub async fn command(&self, parts: Vec<Value>) {
        let _ = send_command(&self.socket, parts).await;
    }

    pub async fn toggle_pause(&self) {
        self.command(vec![json!("cycle"), json!("pause")]).await;
    }
    pub async fn seek(&self, seconds: f64) {
        self.command(vec![json!("seek"), json!(seconds), json!("relative")])
            .await;
    }
    pub async fn add_volume(&self, delta: f64) {
        self.command(vec![json!("add"), json!("volume"), json!(delta)])
            .await;
    }
    pub async fn toggle_mute(&self) {
        self.command(vec![json!("cycle"), json!("mute")]).await;
    }
    pub async fn cycle_subtitle(&self) {
        self.command(vec![json!("cycle"), json!("sub")]).await;
    }
    pub async fn cycle_audio(&self) {
        self.command(vec![json!("cycle"), json!("aid")]).await;
    }
    pub async fn seek_absolute(&self, seconds: f64) {
        self.command(vec![json!("seek"), json!(seconds), json!("absolute")]).await;
    }

    /// True once mpv has exited (end of file or crash).
    pub fn finished(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Ask mpv to quit, ensure the process is gone, and remove the socket.
    pub async fn stop(mut self) {
        self.gc_task.abort();
        self.command(vec![json!("quit")]).await;
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        let _ = std::fs::remove_file(&self.socket);
        shm_cleanup(&self.shm_before);
    }
}

impl Drop for EmbeddedPlayer {
    fn drop(&mut self) {
        // Best-effort cleanup on unexpected drop (panic / early exit). The
        // child is killed automatically via kill_on_drop; we just clean SHM.
        self.gc_task.abort();
        shm_cleanup(&self.shm_before);
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Names of all entries currently in /dev/shm/.
fn shm_snapshot() -> HashSet<String> {
    std::fs::read_dir("/dev/shm")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Remove /dev/shm entries that were not present in `before` and whose names
/// look like mpv created them. Conservative prefix filter avoids touching
/// unrelated SHM objects from other processes.
fn shm_cleanup(before: &HashSet<String>) {
    if let Ok(rd) = std::fs::read_dir("/dev/shm") {
        for entry in rd.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !before.contains(&name)
                && (name.starts_with("mpv") || name.starts_with("kitty"))
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Like `shm_cleanup` but only removes entries whose modification time is at
/// least `min_age` old. Used during playback so we don't delete a segment
/// mpv just wrote and the terminal hasn't read yet.
fn shm_cleanup_aged(before: &HashSet<String>, min_age: Duration) {
    let now = std::time::SystemTime::now();
    if let Ok(rd) = std::fs::read_dir("/dev/shm") {
        for entry in rd.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !before.contains(&name)
                && (name.starts_with("mpv") || name.starts_with("kitty"))
            {
                let old_enough = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| now.duration_since(t).ok())
                    .is_none_or(|age| age >= min_age);
                if old_enough {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Best-effort `quit` over an mpv IPC socket. Used to close the external window
/// backend, which has no in-process handle. No-op if the socket is gone.
pub(crate) async fn quit(socket: &str) {
    command(socket, vec![json!("quit")]).await;
}

/// Best-effort mpv IPC command over a socket path (external backend, which has no
/// in-process handle). No-op if the socket is gone.
pub(crate) async fn command(socket: &str, parts: Vec<Value>) {
    let _ = send_command(socket, parts).await;
}

async fn send_command(socket: &str, parts: Vec<Value>) -> Result<()> {
    let line = ipc_command(&parts)?;
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| Error::Player(format!("ipc connect: {e}")))?;
    stream
        .write_all(line.as_bytes())
        .await
        .map_err(|e| Error::Player(format!("ipc write: {e}")))?;
    Ok(())
}

/// Wait for mpv to create the IPC socket, then connect.
async fn connect_retry(socket: &str) -> Option<UnixStream> {
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(socket).await {
            return Some(s);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// Observe time-pos/duration/pause and forward one sample per whole second.
/// Reused by the external-window backend so it records resume progress too.
pub(crate) async fn observe(socket: String, tx: mpsc::UnboundedSender<ProgressUpdate>) {
    let Some(stream) = connect_retry(&socket).await else {
        return;
    };
    let (read_half, mut write_half) = stream.into_split();

    for (id, name) in [(1u64, "time-pos"), (2, "duration"), (3, "pause")] {
        if let Ok(line) = ipc_command(&[json!("observe_property"), json!(id), json!(name)]) {
            let _ = write_half.write_all(line.as_bytes()).await;
        }
    }

    let mut lines = BufReader::new(read_half).lines();
    let mut duration = 0.0_f64;
    let mut paused = false;
    let mut last_sec = i64::MIN;

    // Rolling fps measurement: track wall-clock times of the last N time-pos events.
    let mut frame_times: std::collections::VecDeque<std::time::Instant> =
        std::collections::VecDeque::new();
    let mut current_fps: Option<f64> = None;

    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("event").and_then(Value::as_str) != Some("property-change") {
            continue;
        }
        match v.get("name").and_then(Value::as_str) {
            Some("duration") => {
                if let Some(d) = v.get("data").and_then(Value::as_f64) {
                    duration = d;
                }
            }
            Some("pause") => {
                if let Some(p) = v.get("data").and_then(Value::as_bool) {
                    paused = p;
                }
            }
            Some("time-pos") => {
                if let Some(pos) = v.get("data").and_then(Value::as_f64) {
                    // Measure fps from the arrival rate of time-pos events (one per frame).
                    let now = std::time::Instant::now();
                    frame_times.push_back(now);
                    if frame_times.len() > 30 {
                        frame_times.pop_front();
                    }
                    if frame_times.len() >= 2 {
                        let elapsed = frame_times
                            .back()
                            .unwrap()
                            .duration_since(*frame_times.front().unwrap())
                            .as_secs_f64();
                        if elapsed > 0.1 {
                            current_fps =
                                Some((frame_times.len() - 1) as f64 / elapsed);
                        }
                    }

                    let sec = pos as i64;
                    if sec != last_sec {
                        last_sec = sec;
                        let update = ProgressUpdate {
                            position: pos,
                            duration,
                            paused,
                            fps: current_fps,
                        };
                        if tx.send(update).is_err() {
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
