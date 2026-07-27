//! Input mapping: raw key events -> typed [`Action`]s. Keeping this separate
//! makes bindings configurable and lets state-transition tests drive the app
//! without a real terminal.

use crossterm::event::{KeyCode, KeyModifiers};
use serde::{Deserialize, Serialize};

/// Everything the user can ask the app to do. The event loop consumes these;
/// it never inspects raw keycodes directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Quit,
    Up,
    Down,
    Left,
    Right,
    Select,
    Back,
    Search,
    Tab,
    ToggleFavourite,
    /// Cycle catalogue sort order (browse views).
    CycleSort,
    /// Enter the client-side quick-filter capture (browse views).
    Filter,
    /// Open the source picker for the selected episode (Episodes view). Plain Enter
    /// plays the default source directly; this key lets you choose another.
    ChooseSource,
    // Playback
    PlayPause,
    SeekForward,
    SeekBackward,
    VolumeUp,
    VolumeDown,
    Mute,
    NextEpisode,
    PrevEpisode,
    CycleSubtitle,
    CycleAudio,
    StopPlayback,
}

/// Default keymap. Vim-style motion plus intuitive playback keys. A future
/// config loader can override this from `[keys]` in config.toml.
pub fn default_binding(code: KeyCode, mods: KeyModifiers) -> Option<Action> {
    use KeyCode::*;
    if mods.contains(KeyModifiers::CONTROL) && matches!(code, Char('c')) {
        return Some(Action::Quit);
    }
    let a = match code {
        Char('q') => Action::Quit,
        Char('k') | Up => Action::Up,
        Char('j') | Down => Action::Down,
        Char('h') | Left => Action::Left,
        Char('l') | Right => Action::Right,
        Enter => Action::Select,
        Esc => Action::Back,
        Char('/') => Action::Search,
        Tab => Action::Tab,
        Char('f') => Action::ToggleFavourite,
        Char('S') => Action::CycleSort,
        Char('F') => Action::Filter,
        Char('c') => Action::ChooseSource,
        Char(' ') => Action::PlayPause,
        Char('m') => Action::Mute,
        Char('n') => Action::NextEpisode,
        Char('p') => Action::PrevEpisode,
        Char('s') => Action::CycleSubtitle,
        Char('a') => Action::CycleAudio,
        _ => return None,
    };
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vim_keys_map_to_motion() {
        assert_eq!(default_binding(KeyCode::Char('j'), KeyModifiers::NONE), Some(Action::Down));
        assert_eq!(default_binding(KeyCode::Char('k'), KeyModifiers::NONE), Some(Action::Up));
    }

    #[test]
    fn sort_and_filter_bindings() {
        assert_eq!(default_binding(KeyCode::Char('S'), KeyModifiers::NONE), Some(Action::CycleSort));
        assert_eq!(default_binding(KeyCode::Char('F'), KeyModifiers::NONE), Some(Action::Filter));
        assert_eq!(default_binding(KeyCode::Char('c'), KeyModifiers::NONE), Some(Action::ChooseSource));
    }

    #[test]
    fn ctrl_c_quits() {
        assert_eq!(
            default_binding(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Action::Quit)
        );
    }
}
