//! Central application state and the typed view/navigation state machine.
//!
//! Transitions are pure: [`App::on_action`] mutates state and returns an
//! [`Effect`] describing any side effect for the async loop to perform. This
//! keeps all navigation logic unit-testable with no terminal and no network.

pub mod run;

use std::collections::{HashMap, HashSet};

use ratatui::widgets::ListState;

use crate::input::Action;
use crate::models::{AnimeDetails, AnimeId, AnimeSummary, EpisodeId};

/// One visible line in the Episodes view: either a foldable season header or an
/// episode belonging to an expanded season. Episodes are indexed into
/// `details.episodes`. Built by [`App::episode_rows`] from the current
/// expand/collapse state; single-season titles produce only `Episode` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeRow {
    Season { id: u32, rank: usize, expanded: bool, episode_count: usize },
    Episode { index: usize, indented: bool },
}

/// Top-level screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Home,
    Search,
    Sources,
    Details,
    Episodes,
    Favourites,
    History,
    Player,
}

/// Live playback readout mirrored from mpv's IPC property observation.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlaybackState {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub fps: Option<f64>,
}

impl PlaybackState {
    /// Fraction 0.0..=1.0 for a progress gauge.
    pub fn ratio(&self) -> f64 {
        if self.duration > 0.0 {
            (self.position / self.duration).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// Format seconds as H:MM:SS or M:SS.
pub fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// Side effects the async loop performs after a state transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Search(String),
    LoadDetails(AnimeId),
    LoadFavourites,
    LoadHistory,
    Play(AnimeId, EpisodeId),
    /// User confirmed a source from the source-selection list (index into runner's pending list).
    SelectSource(usize),
    /// Toggle favourite for the current anime.
    ToggleFavourite(AnimeId, String),
}

pub struct App {
    pub view: View,
    pub should_quit: bool,
    pub status: Option<String>,
    pub loading: bool,

    /// True while the search box is capturing text.
    pub input_mode: bool,
    pub search_input: String,

    /// Catalogue results shown in Home/Search/Favourites/History.
    pub results: Vec<AnimeSummary>,
    pub results_state: ListState,

    /// Loaded details for the selected title.
    pub details: Option<AnimeDetails>,
    /// Selection in the Episodes view. NOTE: this indexes VISIBLE ROWS
    /// ([`App::episode_rows`]), not `details.episodes` directly — a row may be a
    /// season header. Use [`App::selected_episode_index`] to get an episode.
    pub episodes_state: ListState,

    /// Season ids currently expanded (folded-open) in the Episodes view.
    pub expanded_seasons: HashSet<u32>,

    /// Resume positions keyed by episode_id (populated when details load).
    pub resume_positions: HashMap<String, f64>,

    /// Whether the current anime is in the user's favourites.
    pub is_favourite: bool,

    /// Labels for the source-selection list (runner holds the actual URLs).
    pub source_labels: Vec<String>,
    pub source_state: ListState,

    /// Live playback readout while in [`View::Player`].
    pub playback: Option<PlaybackState>,

    /// Video fills the entire terminal (no chrome).
    pub fullscreen: bool,

    /// Some(s) while the user is typing a seek-to-time value; None otherwise.
    pub seek_input: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            view: View::Home,
            should_quit: false,
            status: None,
            loading: false,
            input_mode: false,
            search_input: String::new(),
            results: Vec::new(),
            results_state: ListState::default(),
            details: None,
            episodes_state: ListState::default(),
            expanded_seasons: HashSet::new(),
            resume_positions: HashMap::new(),
            is_favourite: false,
            source_labels: Vec::new(),
            source_state: ListState::default(),
            playback: None,
            fullscreen: false,
            seek_input: None,
        }
    }
}

impl App {
    pub fn on_action(&mut self, action: Action) -> Effect {
        match action {
            Action::Quit => {
                self.should_quit = true;
                Effect::None
            }
            Action::Up => {
                self.move_selection(-1);
                Effect::None
            }
            Action::Down => {
                self.move_selection(1);
                Effect::None
            }
            Action::Tab => self.cycle_tab(),
            Action::Search => {
                self.goto(View::Search);
                self.input_mode = true;
                self.search_input.clear();
                Effect::None
            }
            Action::Back | Action::Left => {
                self.back();
                Effect::None
            }
            Action::Right => self.on_select(),
            Action::Select => self.on_select(),
            Action::StopPlayback if self.view == View::Player => {
                self.back();
                Effect::None
            }
            Action::ToggleFavourite => {
                if let Some(d) = &self.details {
                    let id = d.id.clone();
                    let title = d.title.clone();
                    self.is_favourite = !self.is_favourite;
                    Effect::ToggleFavourite(id, title)
                } else {
                    Effect::None
                }
            }
            _ => Effect::None,
        }
    }

    fn on_select(&mut self) -> Effect {
        match self.view {
            View::Home | View::Search | View::Favourites | View::History => {
                match self.selected_result() {
                    Some(anime) => {
                        let id = anime.id.clone();
                        self.goto(View::Details);
                        self.loading = true;
                        self.details = None;
                        self.is_favourite = false;
                        self.resume_positions.clear();
                        Effect::LoadDetails(id)
                    }
                    None => Effect::None,
                }
            }
            View::Details => {
                if self.details.is_some() {
                    self.goto(View::Episodes);
                    if self.episodes_state.selected().is_none() {
                        self.episodes_state.select(Some(0));
                    }
                }
                Effect::None
            }
            View::Episodes => {
                let rows = self.episode_rows();
                match self.episodes_state.selected().and_then(|i| rows.get(i)) {
                    // Season header: fold/unfold. The header stays at the same row
                    // index, so the cursor doesn't jump.
                    Some(EpisodeRow::Season { id, .. }) => {
                        let id = *id;
                        if !self.expanded_seasons.remove(&id) {
                            self.expanded_seasons.insert(id);
                        }
                        Effect::None
                    }
                    Some(EpisodeRow::Episode { index, .. }) => {
                        let index = *index;
                        match self.details.as_ref() {
                            Some(d) if index < d.episodes.len() => {
                                let effect =
                                    Effect::Play(d.id.clone(), d.episodes[index].id.clone());
                                self.goto(View::Player);
                                self.loading = true;
                                effect
                            }
                            _ => Effect::None,
                        }
                    }
                    None => Effect::None,
                }
            }
            View::Sources => match self.source_state.selected() {
                Some(i) if i < self.source_labels.len() => Effect::SelectSource(i),
                _ => Effect::None,
            },
            View::Player => Effect::None,
        }
    }

    /// Cycle between the three main list views: Home → Favourites → History → Home.
    fn cycle_tab(&mut self) -> Effect {
        match self.view {
            View::Home | View::Search => {
                self.goto(View::Favourites);
                self.results.clear();
                self.loading = true;
                Effect::LoadFavourites
            }
            View::Favourites => {
                self.goto(View::History);
                self.results.clear();
                self.loading = true;
                Effect::LoadHistory
            }
            View::History => {
                self.goto(View::Home);
                self.results.clear();
                self.loading = true;
                Effect::Search(String::new())
            }
            _ => Effect::None,
        }
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = Some(status.into());
    }

    pub fn selected_result(&self) -> Option<&AnimeSummary> {
        self.results_state.selected().and_then(|i| self.results.get(i))
    }

    pub fn set_results(&mut self, results: Vec<AnimeSummary>) {
        self.loading = false;
        self.results = results;
        self.results_state.select((!self.results.is_empty()).then_some(0));
    }

    pub fn set_details(&mut self, details: AnimeDetails) {
        self.loading = false;
        self.details = Some(details);
        // Multi-season titles start fully folded: the list shows just the season
        // headers so you can open the one you want. Place the cursor on the first
        // visible row (the first season header, or the first episode when there is
        // only one season and thus no headers).
        self.expanded_seasons.clear();
        let rows = self.episode_rows();
        let sel = rows
            .iter()
            .position(|r| matches!(r, EpisodeRow::Episode { .. }))
            .or_else(|| (!rows.is_empty()).then_some(0));
        self.episodes_state.select(sel);
    }

    /// Distinct season ids present in the loaded episodes, sorted. Episodes with
    /// no `season_id` are grouped under a synthetic season `0`.
    fn season_ids(&self) -> Vec<u32> {
        let Some(d) = &self.details else {
            return Vec::new();
        };
        let mut ids: Vec<u32> = d
            .episodes
            .iter()
            .map(|e| e.season_id.unwrap_or(0))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        ids.sort_unstable();
        ids
    }

    /// The visible rows of the Episodes view given the current fold state.
    /// Single-season titles yield only [`EpisodeRow::Episode`] rows (no headers);
    /// multi-season titles yield season headers with their episodes nested when
    /// expanded. Episodes are already globally sorted by (season, number).
    pub fn episode_rows(&self) -> Vec<EpisodeRow> {
        let Some(d) = &self.details else {
            return Vec::new();
        };
        let ids = self.season_ids();
        if ids.len() <= 1 {
            return d
                .episodes
                .iter()
                .enumerate()
                .map(|(index, _)| EpisodeRow::Episode { index, indented: false })
                .collect();
        }
        let mut rows = Vec::new();
        for (rank0, id) in ids.iter().enumerate() {
            let expanded = self.expanded_seasons.contains(id);
            let episode_count = d
                .episodes
                .iter()
                .filter(|e| e.season_id.unwrap_or(0) == *id)
                .count();
            rows.push(EpisodeRow::Season {
                id: *id,
                rank: rank0 + 1,
                expanded,
                episode_count,
            });
            if expanded {
                for (index, _) in d
                    .episodes
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.season_id.unwrap_or(0) == *id)
                {
                    rows.push(EpisodeRow::Episode { index, indented: true });
                }
            }
        }
        rows
    }

    /// Episode index for the currently selected row, or `None` if a season header
    /// (or nothing) is selected.
    pub fn selected_episode_index(&self) -> Option<usize> {
        let rows = self.episode_rows();
        match rows.get(self.episodes_state.selected()?)? {
            EpisodeRow::Episode { index, .. } => Some(*index),
            EpisodeRow::Season { .. } => None,
        }
    }

    /// Expand the season containing `ep_index` and move the cursor onto that
    /// episode's row. Used when switching episodes during playback (n/p keys).
    pub fn select_episode(&mut self, ep_index: usize) {
        let sid = self
            .details
            .as_ref()
            .and_then(|d| d.episodes.get(ep_index))
            .map(|e| e.season_id.unwrap_or(0));
        if let Some(sid) = sid {
            self.expanded_seasons.insert(sid);
        }
        let row = self
            .episode_rows()
            .iter()
            .position(|r| matches!(r, EpisodeRow::Episode { index, .. } if *index == ep_index));
        self.episodes_state.select(row.or(Some(0)));
    }

    pub fn set_sources(&mut self, labels: Vec<String>) {
        self.source_labels = labels;
        let sel = (!self.source_labels.is_empty()).then_some(0);
        self.source_state.select(sel);
        if !self.source_labels.is_empty() {
            self.goto(View::Sources);
        }
    }

    fn move_selection(&mut self, delta: isize) {
        // Compute length first (episode_rows borrows &self) before taking the
        // &mut to the ListState, so the borrows don't overlap.
        let len = match self.view {
            View::Details | View::Episodes => self.episode_rows().len(),
            View::Sources => self.source_labels.len(),
            _ => self.results.len(),
        };
        if len == 0 {
            return;
        }
        let state = match self.view {
            View::Details | View::Episodes => &mut self.episodes_state,
            View::Sources => &mut self.source_state,
            _ => &mut self.results_state,
        };
        let current = state.selected().unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(len as isize) as usize;
        state.select(Some(next));
    }

    fn goto(&mut self, view: View) {
        self.view = view;
    }

    fn back(&mut self) {
        self.input_mode = false;
        self.view = match self.view {
            View::Home | View::Favourites | View::History => View::Home,
            View::Search => View::Home,
            View::Details => View::Home,
            View::Episodes => View::Details,
            View::Sources => View::Episodes,
            View::Player => View::Episodes,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AnimeDetails, Episode};

    fn seed_results(app: &mut App) {
        app.set_results(vec![AnimeSummary {
            id: AnimeId("a1".into()),
            title: "T".into(),
            poster_url: None,
            year: None,
        }]);
    }

    fn seed_details(app: &mut App) {
        app.set_details(AnimeDetails {
            id: AnimeId("a1".into()),
            title: "T".into(),
            description: None,
            poster_url: None,
            genres: vec![],
            status: None,
            episodes: vec![Episode {
                id: EpisodeId("e1".into()),
                number: "1".into(),
                title: None,
                season_id: None,
            }],
        });
    }

    fn seed_multi_season(app: &mut App) {
        let ep = |id: &str, num: &str, season: u32| Episode {
            id: EpisodeId(id.into()),
            number: num.into(),
            title: None,
            season_id: Some(season),
        };
        app.set_details(AnimeDetails {
            id: AnimeId("a1".into()),
            title: "T".into(),
            description: None,
            poster_url: None,
            genres: vec![],
            status: None,
            // Already sorted by (season, number) as the provider guarantees.
            episodes: vec![
                ep("s1e1", "1", 10),
                ep("s1e2", "2", 10),
                ep("s2e1", "1", 20),
                ep("s2e2", "2", 20),
            ],
        });
    }

    #[test]
    fn multi_season_starts_fully_folded() {
        let mut app = App::default();
        seed_multi_season(&mut app);
        let rows = app.episode_rows();
        // Only the two season headers are visible; both collapsed.
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0], EpisodeRow::Season { rank: 1, expanded: false, episode_count: 2, .. }));
        assert!(matches!(rows[1], EpisodeRow::Season { rank: 2, expanded: false, episode_count: 2, .. }));
        // Cursor starts on the first season header (no episode selected).
        assert_eq!(app.episodes_state.selected(), Some(0));
        assert_eq!(app.selected_episode_index(), None);
    }

    #[test]
    fn selecting_season_header_toggles_fold() {
        let mut app = App::default();
        seed_multi_season(&mut app);
        app.goto(View::Episodes);
        // Cursor starts on the first season header (row 0); expand it.
        assert_eq!(app.episodes_state.selected(), Some(0));
        assert_eq!(app.on_action(Action::Select), Effect::None);
        let rows = app.episode_rows();
        assert!(matches!(rows[0], EpisodeRow::Season { rank: 1, expanded: true, .. }));
        assert!(matches!(rows[1], EpisodeRow::Episode { index: 0, .. }));
        assert!(matches!(rows[2], EpisodeRow::Episode { index: 1, .. }));
        // Selecting it again folds it back to just headers.
        assert_eq!(app.on_action(Action::Select), Effect::None);
        assert_eq!(app.episode_rows().len(), 2);
    }

    #[test]
    fn playing_episode_in_collapsed_season_expands_it() {
        let mut app = App::default();
        seed_multi_season(&mut app);
        // Episode index 2 lives in the initially-collapsed second season.
        app.select_episode(2);
        assert_eq!(app.selected_episode_index(), Some(2));
        assert!(app.expanded_seasons.contains(&20));
    }

    #[test]
    fn select_result_requests_details() {
        let mut app = App::default();
        seed_results(&mut app);
        let eff = app.on_action(Action::Select);
        assert_eq!(app.view, View::Details);
        assert_eq!(eff, Effect::LoadDetails(AnimeId("a1".into())));
    }

    #[test]
    fn select_without_results_is_noop() {
        let mut app = App::default();
        assert_eq!(app.on_action(Action::Select), Effect::None);
        assert_eq!(app.view, View::Home);
    }

    #[test]
    fn drill_to_play_then_back_unwinds() {
        let mut app = App::default();
        seed_results(&mut app);
        app.on_action(Action::Select); // -> Details
        seed_details(&mut app);
        assert_eq!(app.on_action(Action::Select), Effect::None); // -> Episodes
        assert_eq!(app.view, View::Episodes);
        let eff = app.on_action(Action::Select); // -> Player
        assert_eq!(app.view, View::Player);
        assert_eq!(eff, Effect::Play(AnimeId("a1".into()), EpisodeId("e1".into())));
        app.on_action(Action::Back);
        assert_eq!(app.view, View::Episodes);
    }

    #[test]
    fn source_selection_flow() {
        let mut app = App::default();
        app.set_sources(vec!["vidmoly (VF)".into(), "sibnet (VOSTFR)".into()]);
        assert_eq!(app.view, View::Sources);
        assert_eq!(app.source_state.selected(), Some(0));
        let eff = app.on_action(Action::Select);
        assert_eq!(eff, Effect::SelectSource(0));
        app.on_action(Action::Down);
        let eff = app.on_action(Action::Select);
        assert_eq!(eff, Effect::SelectSource(1));
    }

    #[test]
    fn tab_cycles_home_favourites_history() {
        let mut app = App::default();
        let eff = app.on_action(Action::Tab);
        assert_eq!(app.view, View::Favourites);
        assert_eq!(eff, Effect::LoadFavourites);
        let eff = app.on_action(Action::Tab);
        assert_eq!(app.view, View::History);
        assert_eq!(eff, Effect::LoadHistory);
        let eff = app.on_action(Action::Tab);
        assert_eq!(app.view, View::Home);
        assert!(matches!(eff, Effect::Search(_)));
    }

    #[test]
    fn toggle_favourite_flips_flag_and_returns_effect() {
        let mut app = App::default();
        seed_results(&mut app);
        app.on_action(Action::Select); // -> Details
        seed_details(&mut app);
        let eff = app.on_action(Action::ToggleFavourite);
        assert!(app.is_favourite);
        assert_eq!(eff, Effect::ToggleFavourite(AnimeId("a1".into()), "T".into()));
        app.on_action(Action::ToggleFavourite);
        assert!(!app.is_favourite);
    }

    #[test]
    fn search_enters_input_mode() {
        let mut app = App::default();
        app.on_action(Action::Search);
        assert_eq!(app.view, View::Search);
        assert!(app.input_mode);
    }

    #[test]
    fn navigation_wraps() {
        let mut app = App::default();
        seed_results(&mut app);
        app.on_action(Action::Down);
        assert_eq!(app.results_state.selected(), Some(0));
        app.on_action(Action::Up);
        assert_eq!(app.results_state.selected(), Some(0));
    }

    #[test]
    fn quit_sets_flag() {
        let mut app = App::default();
        app.on_action(Action::Quit);
        assert!(app.should_quit);
    }
}
