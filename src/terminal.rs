//! Terminal lifecycle: enter raw mode + alternate screen, and guarantee full
//! restoration (raw mode, alt screen, cursor, Kitty placements) on every exit
//! path via a `Drop` guard plus a panic hook. Shared by the app and the player.

use std::io::{Stdout, Write};

use crossterm::event::EnableFocusChange;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, cursor};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::errors::Result;
use crate::player::kitty::DELETE_ALL_IMAGES;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Owns the terminal's raw/alt-screen state and restores it on drop.
pub struct TerminalGuard {
    pub terminal: Tui,
}

impl TerminalGuard {
    pub fn new() -> Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        // EnableFocusChange: Ghostty repaints from its image cache on focus
        // loss/gain (workspace switch); if mpv keeps transmitting frames during
        // that repaint, RSS spikes to GBs. We pause mpv on FocusLost / resume on
        // FocusGained (see Runner::on_terminal_event) to avoid it.
        execute!(stdout, EnterAlternateScreen, cursor::Hide, EnableFocusChange)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Sequences that disable terminal input modes mpv may turn on (mouse tracking
/// in all encodings, focus reporting, bracketed paste). mpv is killed abruptly
/// on our exit paths, so it never resets these itself — without this the shell
/// is flooded with mouse escape sequences (e.g. `CQ:`/`CN:`) after we quit.
const DISABLE_INPUT_MODES: &str =
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?1004l\x1b[?2004l";

/// Idempotent terminal restoration. Safe to call multiple times / from a hook.
pub fn restore() {
    let mut out = std::io::stdout();
    let _ = out.write_all(DELETE_ALL_IMAGES.as_bytes());
    let _ = out.write_all(DISABLE_INPUT_MODES.as_bytes());
    let _ = out.flush();
    let _ = execute!(out, LeaveAlternateScreen, cursor::Show);
    let _ = disable_raw_mode();
}

/// Restore the terminal before the default panic message prints, so a panic
/// during the TUI doesn't leave the user with a garbled terminal.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        default(info);
    }));
}
