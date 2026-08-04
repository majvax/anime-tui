//! Playback proof-of-concept (Phase 1 validation).
//!
//! Validates the embedded-playback pipeline end to end:
//!   1. Reserve a rectangle in a ratatui UI.
//!   2. Spawn mpv with `--vo=kitty` confined to that rectangle + JSON IPC.
//!   3. mpv decodes, plays synchronized audio, and paints frames via the Kitty
//!      graphics protocol into the reserved cells.
//!   4. Space toggles pause over the IPC socket; `r`-esize respawns mpv aligned
//!      to the new rectangle; `q` quits.
//!   5. On EVERY exit path: kill mpv, delete Kitty image placements, restore the
//!      terminal (raw mode, alt screen, cursor).
//!
//! Run inside a Kitty-graphics terminal (Kitty, Ghostty, WezTerm):
//!   cargo run --bin poc_kitty -- /path/to/local.mp4
//!
//! NOTE: this is a local-file POC — no network, no provider. It plays whatever
//! path you pass. It cannot be visually verified from a non-graphics terminal.

use std::io::{Stdout, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use anime_tui::player::kitty::{CellRect, DELETE_ALL_IMAGES};
use anime_tui::player::mpv::{build_args, ipc_command, MpvTuning, Presentation};
use anime_tui::ui::video_rect;

const SOCKET: &str = "/tmp/anime-tui-poc-mpv.sock";

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: poc_kitty <path-to-local-video>");
            eprintln!("hint:  scripts/gen_test_media.sh creates /tmp/anime-tui-poc-test.mp4");
            std::process::exit(2);
        }
    };

    if let Err(e) = run(&path) {
        eprintln!("poc_kitty error: {e}");
        std::process::exit(1);
    }
}

/// Owns terminal + child mpv and guarantees cleanup via Drop, so panics and
/// early returns still restore the terminal and delete Kitty placements.
struct Session {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    child: Option<Child>,
}

impl Session {
    fn new() -> std::io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal, child: None })
    }

    fn kill_mpv(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill_mpv();
        // Delete every Kitty image placement, then restore the terminal.
        let mut out = std::io::stdout();
        let _ = out.write_all(DELETE_ALL_IMAGES.as_bytes());
        let _ = out.flush();
        let _ = out.execute(LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = std::fs::remove_file(SOCKET);
    }
}

fn run(path: &str) -> std::io::Result<()> {
    // Restore the terminal even on panic before unwinding.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = std::io::stdout();
        let _ = out.write_all(DELETE_ALL_IMAGES.as_bytes());
        let _ = out.flush();
        let _ = disable_raw_mode();
        let _ = execute!(out, LeaveAlternateScreen);
        default_hook(info);
    }));

    let mut session = Session::new()?;
    let _ = std::fs::remove_file(SOCKET);

    let mut rect = draw(&mut session)?;
    session.child = Some(spawn_mpv(path, rect)?);

    loop {
        // mpv exited (end of file / crash) -> leave.
        if let Some(child) = session.child.as_mut() {
            if let Ok(Some(_status)) = child.try_wait() {
                break;
            }
        }

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char(' ') => toggle_pause(),
                    _ => {}
                },
                Event::Resize(_, _) => {
                    // Respawn mpv aligned to the new reserved rectangle. Delete
                    // old placements first so no stale frame lingers.
                    let mut out = std::io::stdout();
                    let _ = out.write_all(DELETE_ALL_IMAGES.as_bytes());
                    let _ = out.flush();
                    session.kill_mpv();
                    rect = draw(&mut session)?;
                    session.child = Some(spawn_mpv(path, rect)?);
                }
                _ => {}
            }
        }
    }

    // Explicit cleanup happens here and again (idempotently) in Drop.
    session.kill_mpv();
    Ok(())
}

/// Draw the POC chrome and return the reserved video rectangle (in cells).
fn draw(session: &mut Terminalish) -> std::io::Result<CellRect> {
    let mut cell = CellRect { left: 0, top: 0, cols: 0, rows: 0, pixel_width: None, pixel_height: None };
    session.terminal.draw(|frame| {
        use ratatui::layout::{Constraint, Layout};
        use ratatui::widgets::{Block, Borders, Paragraph};
        let area = frame.area();
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
        frame.render_widget(
            Paragraph::new(" poc_kitty — space: pause · r: (auto on resize) · q: quit "),
            rows[0],
        );
        // rows[1] reserved for mpv's Kitty output — draw only a thin border so
        // the interior is never overpainted.
        frame.render_widget(Block::default().borders(Borders::NONE), rows[1]);
        frame.render_widget(Paragraph::new(" video surface above (mpv --vo=kitty) "), rows[2]);

        let r = video_rect(area, false);
        cell = r;
    })?;
    Ok(cell)
}

// Alias so `draw` can borrow just the terminal-bearing part of Session.
type Terminalish = Session;

fn spawn_mpv(path: &str, rect: CellRect) -> std::io::Result<Child> {
    let args = build_args(
        path,
        SOCKET,
        &Presentation::EmbeddedKitty { rect },
        &[],
        &MpvTuning::default(),
    );
    Command::new("mpv")
        .args(&args)
        .stdin(Stdio::null()) // never fight the TUI for the keyboard
        .stdout(Stdio::inherit()) // Kitty graphics escapes go to our terminal
        .stderr(Stdio::null())
        .spawn()
}

/// Toggle pause over the mpv IPC socket (best effort; ignored if not up yet).
fn toggle_pause() {
    if let Ok(mut stream) = UnixStream::connect(SOCKET) {
        if let Ok(cmd) = ipc_command(&[
            serde_json::Value::String("cycle".into()),
            serde_json::Value::String("pause".into()),
        ]) {
            let _ = stream.write_all(cmd.as_bytes());
        }
    }
}
