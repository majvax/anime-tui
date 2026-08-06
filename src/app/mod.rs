//! Central application state and the typed view/navigation state machine.
//!
//! Transitions are pure: [`App::on_action`] mutates state and returns an
//! [`Effect`] describing any side effect for the async loop to perform. This
//! keeps all navigation logic unit-testable with no terminal and no network.

pub mod run;

use std::collections::{HashMap, HashSet};

use ratatui::widgets::ListState;

use crate::input::Action;
use crate::models::{AnimeDetails, AnimeId, AnimeSummary, CatalogPage, EpisodeId};

/// Catalogue ordering. Each maps to a provider-validated `sort` query value; the
/// server does the sorting so it stays consistent across paginated results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    Relevance,
    TitleAsc,
    YearDesc,
    YearAsc,
    Popularity,
    Trending,
    Score,
}

impl SortMode {
    /// The provider `sort` query value.
    pub fn param(self) -> &'static str {
        match self {
            SortMode::Relevance => "relevance",
            SortMode::TitleAsc => "title_asc",
            SortMode::YearDesc => "year_desc",
            SortMode::YearAsc => "year_asc",
            SortMode::Popularity => "popularity",
            SortMode::Trending => "trending",
            SortMode::Score => "score",
        }
    }

    /// Short human label shown in the browse header.
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Relevance => "relevance",
            SortMode::TitleAsc => "title A→Z",
            SortMode::YearDesc => "newest",
            SortMode::YearAsc => "oldest",
            SortMode::Popularity => "popular",
            SortMode::Trending => "trending",
            SortMode::Score => "score",
        }
    }

    /// Next mode in the cycle (wraps).
    pub fn next(self) -> SortMode {
        match self {
            SortMode::Relevance => SortMode::TitleAsc,
            SortMode::TitleAsc => SortMode::YearDesc,
            SortMode::YearDesc => SortMode::YearAsc,
            SortMode::YearAsc => SortMode::Popularity,
            SortMode::Popularity => SortMode::Trending,
            SortMode::Trending => SortMode::Score,
            SortMode::Score => SortMode::Relevance,
        }
    }
}

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
    Downloaded,
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
    LoadDownloads,
    Play(AnimeId, EpisodeId),
    /// Play the episode from the start, ignoring any saved resume position.
    Replay(AnimeId, EpisodeId),
    /// Download the episode for offline playback.
    Download(AnimeId, EpisodeId),
    /// Download a specific source chosen from the source picker (index into the
    /// runner's pending list), rather than the default.
    DownloadSource(usize),
    /// Delete the downloaded file for the episode.
    RemoveDownload(AnimeId, EpisodeId),
    /// Play but always show the source picker (Enter plays the default directly).
    PlayChoose(AnimeId, EpisodeId),
    /// User confirmed a source from the source-selection list (index into runner's pending list).
    SelectSource(usize),
    /// Toggle favourite for the current anime.
    ToggleFavourite(AnimeId, String),
}

/// Severity of a transient status message, used to colour it in the footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusLevel {
    #[default]
    Info,
    Error,
}

pub struct App {
    pub view: View,
    pub should_quit: bool,
    pub status: Option<String>,
    /// Severity of the current `status` (drives its colour).
    pub status_level: StatusLevel,
    /// Ticks until the current `status` auto-clears (0 = none). Decremented by
    /// [`App::tick_status`] on the app tick so messages don't linger forever.
    pub status_ticks: u16,
    /// Frame counter for the loading spinner, advanced once per tick.
    pub spinner_frame: usize,
    pub loading: bool,

    /// True while the search box is capturing text.
    pub input_mode: bool,
    pub search_input: String,

    /// Catalogue results currently shown on screen: `all_results` after the active
    /// client-side `filter`. Every renderer/selection reads THIS list.
    pub results: Vec<AnimeSummary>,
    /// Full fetched accumulation for the current query+sort (all pages loaded so
    /// far). `results` is derived from this by [`App::recompute_results`].
    pub all_results: Vec<AnimeSummary>,
    pub results_state: ListState,
    /// Scroll offset (first visible row) for the browse list, kept by the runner so
    /// per-row thumbnail placement matches what's rendered.
    pub list_offset: usize,

    /// Active catalogue query ("" = full catalogue) and pagination cursor, so the
    /// runner can fetch the next page and the UI can show progress/counts.
    pub query: String,
    pub page: u32,
    pub total_pages: u32,
    pub total: usize,
    /// Server-side ordering for catalogue queries.
    pub sort: SortMode,
    /// Client-side quick-filter over `all_results` (case-insensitive title match).
    pub filter: String,
    /// True while the filter box is capturing text (distinct from `input_mode`'s
    /// server-search capture).
    pub filtering: bool,
    /// True while the next catalogue page is being fetched (shown in the header).
    pub loading_more: bool,

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

    /// Episode ids with a downloaded local file, for the loaded anime (populated
    /// when details load). Drives the downloaded icon and local-first playback.
    pub downloaded: HashSet<String>,
    /// Episode ids with a download currently in flight (drives the in-progress icon).
    pub downloading: HashSet<String>,

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
            status_level: StatusLevel::Info,
            status_ticks: 0,
            spinner_frame: 0,
            loading: false,
            input_mode: false,
            search_input: String::new(),
            results: Vec::new(),
            all_results: Vec::new(),
            results_state: ListState::default(),
            list_offset: 0,
            query: String::new(),
            page: 1,
            total_pages: 1,
            total: 0,
            sort: SortMode::default(),
            filter: String::new(),
            filtering: false,
            loading_more: false,
            details: None,
            episodes_state: ListState::default(),
            expanded_seasons: HashSet::new(),
            resume_positions: HashMap::new(),
            downloaded: HashSet::new(),
            downloading: HashSet::new(),
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
            // Server-side sort applies only to catalogue queries (Home/Search); it
            // re-runs the search from page 1 with the new ordering.
            Action::CycleSort if matches!(self.view, View::Home | View::Search) => self.cycle_sort(),
            // Open the source picker for the selected episode (Enter plays default).
            Action::ChooseSource => self.choose_source(),
            // Re-watch the selected episode from the start (Episodes view).
            Action::Replay if self.view == View::Episodes => match self.selected_episode_ids() {
                Some((anime, episode)) => {
                    self.goto(View::Player);
                    self.loading = true;
                    Effect::Replay(anime, episode)
                }
                None => Effect::None,
            },
            // Download / delete the selected episode (Episodes view).
            Action::Download if self.view == View::Episodes => match self.selected_episode_ids() {
                Some((anime, episode)) => Effect::Download(anime, episode),
                None => Effect::None,
            },
            // In the source picker, `d` downloads the highlighted source instead of
            // playing it (Enter plays).
            Action::Download if self.view == View::Sources => match self.source_state.selected() {
                Some(i) if i < self.source_labels.len() => Effect::DownloadSource(i),
                _ => Effect::None,
            },
            Action::RemoveDownload if self.view == View::Episodes => {
                match self.selected_episode_ids() {
                    Some((anime, episode)) => Effect::RemoveDownload(anime, episode),
                    None => Effect::None,
                }
            }
            // Client-side quick-filter is available in any browse list.
            Action::Filter if self.is_browse_view() => {
                self.filtering = true;
                self.input_mode = true;
                self.filter.clear();
                self.recompute_results();
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn on_select(&mut self) -> Effect {
        match self.view {
            View::Home | View::Search | View::Favourites | View::History | View::Downloaded => {
                match self.selected_result() {
                    Some(anime) => {
                        let id = anime.id.clone();
                        self.goto(View::Details);
                        self.loading = true;
                        self.details = None;
                        self.is_favourite = false;
                        self.resume_positions.clear();
                        self.downloaded.clear();
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

    /// Resolve the selected episode's sources and open the picker (rather than
    /// playing the default directly). No-op outside the Episodes view or on a
    /// season header.
    fn choose_source(&mut self) -> Effect {
        if self.view != View::Episodes {
            return Effect::None;
        }
        let rows = self.episode_rows();
        let Some(EpisodeRow::Episode { index, .. }) = self
            .episodes_state
            .selected()
            .and_then(|i| rows.get(i))
            .copied()
        else {
            return Effect::None;
        };
        match self.details.as_ref() {
            Some(d) if index < d.episodes.len() => {
                let effect = Effect::PlayChoose(d.id.clone(), d.episodes[index].id.clone());
                self.loading = true;
                effect
            }
            _ => Effect::None,
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
                self.goto(View::Downloaded);
                self.results.clear();
                self.loading = true;
                Effect::LoadDownloads
            }
            View::Downloaded => {
                self.goto(View::Home);
                self.results.clear();
                self.loading = true;
                Effect::Search(String::new())
            }
            _ => Effect::None,
        }
    }

    /// Default lifetime of a transient status message, in app ticks. At the 250 ms
    /// tick this is ~5 seconds before it auto-clears.
    pub const STATUS_TTL_TICKS: u16 = 20;

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = Some(status.into());
        self.status_level = StatusLevel::Info;
        self.status_ticks = Self::STATUS_TTL_TICKS;
    }

    /// Show an error status (rendered red) that auto-clears like any other.
    pub fn set_error(&mut self, status: impl Into<String>) {
        self.status = Some(status.into());
        self.status_level = StatusLevel::Error;
        self.status_ticks = Self::STATUS_TTL_TICKS;
    }

    /// Age the current status by one tick; clear it (and reset the level) when its
    /// lifetime runs out. No-op when there is no timed status.
    pub fn tick_status(&mut self) {
        if self.status_ticks > 0 {
            self.status_ticks -= 1;
            if self.status_ticks == 0 {
                self.status = None;
                self.status_level = StatusLevel::Info;
            }
        }
    }

    /// Advance the loading-spinner animation by one frame.
    pub fn advance_spinner(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    pub fn selected_result(&self) -> Option<&AnimeSummary> {
        self.results_state.selected().and_then(|i| self.results.get(i))
    }

    /// True for the scrollable browse lists (where filter/sort/thumbnails apply).
    pub fn is_browse_view(&self) -> bool {
        matches!(
            self.view,
            View::Home | View::Search | View::Favourites | View::History | View::Downloaded
        )
    }

    /// Set a non-paginated result list (favourites/history come from the local DB).
    /// Resets the catalogue cursor and clears any active filter.
    pub fn set_results(&mut self, results: Vec<AnimeSummary>) {
        self.loading = false;
        self.loading_more = false;
        self.query.clear();
        self.page = 1;
        self.total_pages = 1;
        self.total = results.len();
        self.filter.clear();
        self.filtering = false;
        self.all_results = results;
        self.list_offset = 0;
        self.recompute_results();
        self.results_state.select((!self.results.is_empty()).then_some(0));
    }

    /// Apply a fetched catalogue page. `append` extends the current accumulation
    /// (infinite scroll); otherwise it replaces it (fresh search / sort change).
    pub fn set_page(&mut self, page: CatalogPage, append: bool) {
        self.loading = false;
        self.loading_more = false;
        self.page = page.page;
        self.total_pages = page.total_pages.max(1);
        self.total = page.total as usize;
        if append {
            self.all_results.extend(page.items);
        } else {
            self.all_results = page.items;
            self.list_offset = 0;
            self.filter.clear();
            self.filtering = false;
        }
        self.recompute_results();
        if !append {
            self.results_state.select((!self.results.is_empty()).then_some(0));
        }
    }

    /// Rebuild `results` from `all_results` under the active filter. Server output
    /// is already ordered, so this only filters. Keeps the selection valid.
    pub fn recompute_results(&mut self) {
        let f = self.filter.to_lowercase();
        self.results = if f.is_empty() {
            self.all_results.clone()
        } else {
            self.all_results
                .iter()
                .filter(|a| a.title.to_lowercase().contains(&f))
                .cloned()
                .collect()
        };
        // Clamp selection + offset to the new length.
        if self.results.is_empty() {
            self.results_state.select(None);
            self.list_offset = 0;
        } else {
            let sel = self.results_state.selected().unwrap_or(0).min(self.results.len() - 1);
            self.results_state.select(Some(sel));
            if self.list_offset >= self.results.len() {
                self.list_offset = 0;
            }
        }
    }

    /// Advance the sort order and request a fresh page-1 query with it.
    pub fn cycle_sort(&mut self) -> Effect {
        self.sort = self.sort.next();
        self.loading = true;
        Effect::Search(self.query.clone())
    }

    /// Append a char to the quick-filter and re-derive `results` live.
    pub fn push_filter_char(&mut self, c: char) {
        self.filter.push(c);
        self.recompute_results();
    }

    /// Delete the last quick-filter char and re-derive `results` live.
    pub fn pop_filter_char(&mut self) {
        self.filter.pop();
        self.recompute_results();
    }

    /// Clear the quick-filter entirely (Esc) and re-derive `results`.
    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.recompute_results();
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

    /// `(anime_id, episode_id)` for the currently selected episode row, or `None`
    /// if a season header / nothing is selected or details aren't loaded. Used by
    /// the replay/download actions.
    pub fn selected_episode_ids(&self) -> Option<(AnimeId, EpisodeId)> {
        let index = self.selected_episode_index()?;
        let d = self.details.as_ref()?;
        let e = d.episodes.get(index)?;
        Some((d.id.clone(), e.id.clone()))
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
            View::Home | View::Favourites | View::History | View::Downloaded => View::Home,
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
    fn picker_download_key_targets_highlighted_source() {
        let mut app = App::default();
        app.set_sources(vec!["vidmoly (VF)".into(), "sibnet (VF)".into()]);
        assert_eq!(app.view, View::Sources);
        // Enter plays the highlighted source; `d` downloads it instead.
        assert_eq!(app.on_action(Action::Select), Effect::SelectSource(0));
        app.on_action(Action::Down);
        assert_eq!(app.on_action(Action::Download), Effect::DownloadSource(1));
    }

    #[test]
    fn tab_cycles_home_favourites_history_downloaded() {
        let mut app = App::default();
        let eff = app.on_action(Action::Tab);
        assert_eq!(app.view, View::Favourites);
        assert_eq!(eff, Effect::LoadFavourites);
        let eff = app.on_action(Action::Tab);
        assert_eq!(app.view, View::History);
        assert_eq!(eff, Effect::LoadHistory);
        let eff = app.on_action(Action::Tab);
        assert_eq!(app.view, View::Downloaded);
        assert_eq!(eff, Effect::LoadDownloads);
        let eff = app.on_action(Action::Tab);
        assert_eq!(app.view, View::Home);
        assert!(matches!(eff, Effect::Search(_)));
    }

    #[test]
    fn replay_and_download_actions_target_selected_episode() {
        let mut app = App::default();
        seed_details(&mut app);
        app.goto(View::Episodes); // single-season: row 0 is episode 0

        let eff = app.on_action(Action::Replay);
        assert_eq!(app.view, View::Player);
        assert_eq!(eff, Effect::Replay(AnimeId("a1".into()), EpisodeId("e1".into())));

        app.goto(View::Episodes);
        assert_eq!(
            app.on_action(Action::Download),
            Effect::Download(AnimeId("a1".into()), EpisodeId("e1".into()))
        );
        assert_eq!(
            app.on_action(Action::RemoveDownload),
            Effect::RemoveDownload(AnimeId("a1".into()), EpisodeId("e1".into()))
        );
    }

    #[test]
    fn replay_outside_episodes_is_noop() {
        let mut app = App::default();
        assert_eq!(app.on_action(Action::Replay), Effect::None);
        assert_eq!(app.view, View::Home);
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

    #[test]
    fn status_auto_clears_after_ttl() {
        let mut app = App::default();
        app.set_status("hello");
        assert_eq!(app.status.as_deref(), Some("hello"));
        assert_eq!(app.status_level, StatusLevel::Info);
        // Tick out its whole lifetime; it should be gone and back to Info.
        for _ in 0..App::STATUS_TTL_TICKS {
            app.tick_status();
        }
        assert!(app.status.is_none());
        assert_eq!(app.status_level, StatusLevel::Info);
    }

    #[test]
    fn set_error_marks_error_level() {
        let mut app = App::default();
        app.set_error("boom");
        assert_eq!(app.status.as_deref(), Some("boom"));
        assert_eq!(app.status_level, StatusLevel::Error);
        assert!(app.status_ticks > 0);
    }

    fn page(ids: &[&str], p: u32, total_pages: u32, total: u32) -> CatalogPage {
        CatalogPage {
            items: ids
                .iter()
                .map(|id| AnimeSummary {
                    id: AnimeId((*id).into()),
                    title: (*id).to_string(),
                    poster_url: None,
                    year: None,
                })
                .collect(),
            page: p,
            total_pages,
            total,
        }
    }

    #[test]
    fn set_page_replace_then_append() {
        let mut app = App::default();
        app.set_page(page(&["a", "b"], 1, 2, 4), false);
        assert_eq!(app.results.len(), 2);
        assert_eq!(app.all_results.len(), 2);
        assert_eq!(app.total, 4);
        assert_eq!(app.total_pages, 2);
        assert_eq!(app.results_state.selected(), Some(0));

        // Appending the next page grows the accumulation, keeps selection.
        app.results_state.select(Some(1));
        app.set_page(page(&["c", "d"], 2, 2, 4), true);
        assert_eq!(app.all_results.len(), 4);
        assert_eq!(app.results.len(), 4);
        assert_eq!(app.page, 2);
        assert_eq!(app.results_state.selected(), Some(1)); // not reset on append
    }

    #[test]
    fn filter_narrows_and_clamps_selection() {
        let mut app = App::default();
        app.set_page(page(&["Naruto", "Bleach", "Nana"], 1, 1, 3), false);
        app.results_state.select(Some(2)); // "Nana"
        app.filter = "na".into();
        app.recompute_results();
        // "Naruto" and "Nana" match (case-insensitive).
        assert_eq!(app.results.len(), 2);
        // Selection clamped into the shorter list.
        assert!(app.results_state.selected().unwrap() < 2);
        app.clear_filter();
        assert_eq!(app.results.len(), 3);
    }

    #[test]
    fn cycle_sort_advances_and_requests_requery() {
        let mut app = App::default();
        app.goto(View::Home);
        app.query = "demon".into();
        assert_eq!(app.sort, SortMode::Relevance);
        let eff = app.on_action(Action::CycleSort);
        assert_eq!(app.sort, SortMode::TitleAsc);
        assert_eq!(eff, Effect::Search("demon".into()));
    }

    #[test]
    fn filter_action_enters_capture_without_changing_view() {
        let mut app = App::default();
        app.goto(View::Home);
        app.on_action(Action::Filter);
        assert!(app.filtering);
        assert!(app.input_mode);
        assert_eq!(app.view, View::Home);
    }

    #[test]
    fn sort_mode_param_and_cycle() {
        assert_eq!(SortMode::YearDesc.param(), "year_desc");
        assert_eq!(SortMode::TitleAsc.param(), "title_asc");
        // next() visits all seven and wraps.
        let mut s = SortMode::Relevance;
        let mut seen = 1;
        loop {
            s = s.next();
            if s == SortMode::Relevance {
                break;
            }
            seen += 1;
        }
        assert_eq!(seen, 7);
    }
}
