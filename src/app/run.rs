//! The central async event loop. Terminal input, a periodic tick, background
//! task results, and playback-progress samples are merged here; network and
//! playback IO run off the render path in Tokio tasks and report back as
//! messages, so drawing never blocks.

use std::io::Write as _;
use std::sync::Arc;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::layout::Rect;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::app::{App, Effect, PlaybackState, View};
use crate::config::Config;
use crate::database::Database;
use crate::errors::{Error, Result};
use crate::input::default_binding;
use crate::models::{AnimeDetails, AnimeId, AnimeSummary, EpisodeId};
use crate::player::embedded::{EmbeddedPlayer, ProgressUpdate};
use crate::player::kitty::DELETE_ALL_IMAGES;
use crate::player::{self, Backend};
use crate::provider::Provider;
use crate::terminal::TerminalGuard;
use crate::ui;

/// A resolved, validated stream ready to hand to mpv. Kept so playback can be
/// respawned on resize without re-resolving.
///
/// `url` is a yt-dlp pre-resolved DIRECT stream (falling back to the validated
/// provider URL if pre-resolution failed). Both backends use it so mpv doesn't
/// run its own yt-dlp pass at launch — that internal pass is the 1-2 s startup
/// delay we avoid.
#[derive(Clone)]
struct PreparedSource {
    url: String,
    headers: Vec<(String, String)>,
    label: Option<String>,
}

/// Results delivered from background tasks back into the loop.
enum Msg {
    Results(Result<Vec<AnimeSummary>>),
    Details(Result<AnimeDetails>),
    Favourites(Result<Vec<AnimeSummary>>),
    History(Result<Vec<AnimeSummary>>),
    Resolved {
        anime: AnimeId,
        episode: EpisodeId,
        result: Result<Vec<PreparedSource>>,
    },
    /// External-backend playback finished (embedded finish is detected via tick).
    ExternalEnded {
        anime: AnimeId,
        episode: EpisodeId,
        result: Result<()>,
    },
    /// Background prefetch of an episode's sources completed (empty on failure).
    Prefetched {
        episode: EpisodeId,
        sources: Vec<PreparedSource>,
    },
    /// Fetched + decoded + re-encoded poster PNG for an anime's details page.
    Poster {
        anime: AnimeId,
        png: Result<Vec<u8>>,
    },
}

pub struct Runner {
    app: App,
    provider: Arc<dyn Provider>,
    db: Database,
    mpv_path: String,
    backend: Backend,
    save_interval: f64,

    tx: mpsc::UnboundedSender<Msg>,
    rx: mpsc::UnboundedReceiver<Msg>,
    prog_tx: mpsc::UnboundedSender<ProgressUpdate>,
    prog_rx: mpsc::UnboundedReceiver<ProgressUpdate>,

    // Active embedded playback (None for external or when idle).
    player: Option<EmbeddedPlayer>,
    playing: Option<(AnimeId, EpisodeId)>,
    current_source: Option<PreparedSource>,
    last_saved_pos: f64,

    /// Runner-side list of validated sources for the current selection request.
    /// The App only holds display labels; the Runner holds the actual URLs.
    pending_sources: Vec<PreparedSource>,

    /// Prefetched, fully-resolved sources keyed by episode id, so pressing Enter
    /// on an episode you were hovering plays instantly. Cleared per anime.
    source_cache: std::collections::HashMap<String, Vec<PreparedSource>>,
    /// Episode id whose prefetch is currently in flight (at most one at a time).
    prefetch_inflight: Option<String>,
    /// Currently-hovered episode + when it became the selection, for debouncing
    /// prefetch so scrolling past episodes doesn't spawn a resolve for each.
    prefetch_target: Option<(String, std::time::Instant)>,
    /// Proactive prefetch queue (resume episode + the next one), warmed as soon
    /// as an anime's details load — drained before hover-based prefetch.
    prefetch_queue: std::collections::VecDeque<(AnimeId, EpisodeId)>,

    /// Seconds the `i` key jumps to skip an opening.
    skip_intro_secs: u64,
    /// On-disk poster cache directory (posters are also decoded/resized to PNG).
    poster_cache: crate::cache::Cache,
    /// The current anime's decoded poster PNG, ready to transmit to the terminal.
    current_poster: Option<(AnimeId, Vec<u8>)>,
    /// Set when the poster must be (re)painted into its reserved rect.
    poster_dirty: bool,
    /// Whether the poster image is currently placed on screen (for clean removal).
    poster_shown: bool,

    /// Set when the video surface must be wiped and the TUI fully repainted
    /// (playback ended / resize). Acted on in `run` where the terminal lives.
    needs_clear: bool,

    /// Last time the TUI chrome was redrawn during embedded playback. Used to
    /// throttle ratatui flushes so they don't compete with mpv's Kitty output.
    last_tui_draw: std::time::Instant,

    /// mpv read-ahead buffer caps (its own RAM, not the terminal image cache).
    tuning: crate::player::mpv::MpvTuning,
}

impl Runner {
    pub fn new(config: &Config, provider: Arc<dyn Provider>, db: Database) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (prog_tx, prog_rx) = mpsc::unbounded_channel();
        let backend = player::select_backend(config);
        let app = App::default();
        let poster_dir = config
            .cache_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("posters");
        let poster_cache = crate::cache::Cache::new(poster_dir)?;
        Ok(Self {
            app,
            provider,
            db,
            mpv_path: config.mpv_path.clone(),
            backend,
            save_interval: config.progress_save_interval_secs as f64,
            tx,
            rx,
            prog_tx,
            prog_rx,
            player: None,
            playing: None,
            current_source: None,
            last_saved_pos: 0.0,
            pending_sources: Vec::new(),
            source_cache: std::collections::HashMap::new(),
            prefetch_inflight: None,
            prefetch_target: None,
            prefetch_queue: std::collections::VecDeque::new(),
            skip_intro_secs: config.playback.skip_intro_secs,
            poster_cache,
            current_poster: None,
            poster_dirty: false,
            poster_shown: false,
            needs_clear: false,
            last_tui_draw: std::time::Instant::now(),
            tuning: crate::player::mpv::MpvTuning {
                max_buffer_mib: config.playback.max_buffer_mib,
                readahead_secs: config.playback.readahead_secs,
            },
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let mut guard = TerminalGuard::new()?;
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));

        // Load the initial catalogue on startup.
        self.app.loading = true;
        self.dispatch(Effect::Search(String::new()));

        guard.terminal.draw(|f| ui::render(f, &self.app))?;

        while !self.app.should_quit {
            tokio::select! {
                maybe_event = events.next() => match maybe_event {
                    Some(Ok(event)) => self.on_terminal_event(event).await,
                    Some(Err(e)) => return Err(Error::Io(e)),
                    None => break,
                },
                Some(msg) = self.rx.recv() => self.on_message(msg).await,
                Some(update) = self.prog_rx.recv() => self.on_progress(update),
                _ = tick.tick() => self.on_tick().await,
            }

            // Wipe a finished/relocated video surface and force a full repaint.
            if self.needs_clear {
                let mut out = std::io::stdout();
                let _ = out.write_all(DELETE_ALL_IMAGES.as_bytes());
                let _ = out.flush();
                guard.terminal.clear()?;
                self.needs_clear = false;
                self.poster_shown = false; // the clear wiped it too
            }

            // During embedded playback, mpv writes Kitty frames to the same
            // stdout as ratatui. Throttle TUI chrome redraws to once per second
            // so they don't compete with video frames and cause stutter.
            let player_active = self.player.is_some();
            let since_last = self.last_tui_draw.elapsed();
            if !player_active || since_last >= std::time::Duration::from_millis(900) {
                guard.terminal.draw(|f| ui::render(f, &self.app))?;
                self.last_tui_draw = std::time::Instant::now();
            }

            // Paint (or remove) the details-page poster into its reserved rect,
            // after the TUI draw so ratatui doesn't clobber it.
            self.render_poster();
        }

        // Stop any embedded playback cleanly before the guard restores the term.
        if let Some(p) = self.player.take() {
            p.stop().await;
        }
        Ok(())
    }

    async fn on_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if self.app.input_mode {
                    self.on_search_key(key.code);
                } else if self.app.view == View::Player {
                    self.on_player_key(key.code).await;
                } else if let Some(action) = default_binding(key.code, key.modifiers) {
                    let effect = self.app.on_action(action);
                    self.dispatch(effect);
                }
            }
            Event::Resize(_, _) => {
                self.poster_dirty = true; // reposition the poster at the new size
                self.on_resize().await;
            }
            _ => {}
        }
    }

    /// Paint the details-page poster into its reserved rect, or remove it when we
    /// leave the details page. Called after the TUI draw so it isn't overpainted.
    fn render_poster(&mut self) {
        use std::io::Write as _;
        if self.app.view == View::Details {
            let Some((_, png)) = &self.current_poster else {
                return;
            };
            if !self.poster_dirty && self.poster_shown {
                return; // already on screen and unchanged
            }
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            let rect = ui::poster_rect(Rect::new(0, 0, cols, rows));
            if rect.is_empty() {
                return;
            }
            let mut out = std::io::stdout();
            let _ = out.write_all(crate::player::kitty::DELETE_POSTER.as_bytes());
            let _ = out.write_all(crate::player::kitty::transmit_png(png, rect).as_bytes());
            let _ = out.flush();
            self.poster_dirty = false;
            self.poster_shown = true;
        } else if self.poster_shown {
            let mut out = std::io::stdout();
            let _ = out.write_all(crate::player::kitty::DELETE_POSTER.as_bytes());
            let _ = out.flush();
            self.poster_shown = false;
        }
    }

    /// Player-view controls go straight to mpv over IPC (embedded); for the
    /// external backend only stop/next/prev are meaningful here.
    async fn on_player_key(&mut self, code: KeyCode) {
        // Seek-to-time input mode intercepts all keys.
        if self.app.seek_input.is_some() {
            self.on_seek_input_key(code).await;
            return;
        }

        // All controls route through `player_command` so they work for BOTH the
        // embedded and external (window) backends.
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.end_playback("stopped").await,
            KeyCode::Char('n') => self.change_episode(1).await,
            KeyCode::Char('p') => self.change_episode(-1).await,
            KeyCode::Char('f') => self.toggle_fullscreen().await,
            KeyCode::Char('g') => {
                self.app.seek_input = Some(String::new());
            }
            // Hand off to a standalone mpv window (no terminal image cache → no
            // multi-GB RSS), resuming at the current position.
            KeyCode::Char('o') => self.open_external().await,
            // Skip an opening.
            KeyCode::Char('i') => self.player_seek(self.skip_intro_secs as f64).await,
            KeyCode::Char(' ') => self.player_command(vec![json!("cycle"), json!("pause")]).await,
            KeyCode::Left | KeyCode::Char('h') => self.player_seek(-10.0).await,
            KeyCode::Right | KeyCode::Char('l') => self.player_seek(10.0).await,
            KeyCode::Char(',') => self.player_seek(-5.0).await,
            KeyCode::Char('.') => self.player_seek(5.0).await,
            KeyCode::Up | KeyCode::Char('+') | KeyCode::Char('=') => {
                self.player_command(vec![json!("add"), json!("volume"), json!(5.0)]).await
            }
            KeyCode::Down | KeyCode::Char('-') => {
                self.player_command(vec![json!("add"), json!("volume"), json!(-5.0)]).await
            }
            KeyCode::Char('m') => self.player_command(vec![json!("cycle"), json!("mute")]).await,
            KeyCode::Char('s') => self.player_command(vec![json!("cycle"), json!("sub")]).await,
            KeyCode::Char('a') => self.player_command(vec![json!("cycle"), json!("aid")]).await,
            _ => {}
        }
    }

    /// Send an mpv IPC command to whichever backend is active (embedded handle, or
    /// the external window's socket).
    async fn player_command(&self, parts: Vec<Value>) {
        if let Some(player) = &self.player {
            player.command(parts).await;
        } else {
            crate::player::embedded::command(&player::external_socket_path(), parts).await;
        }
    }

    async fn player_seek(&self, secs: f64) {
        self.player_command(vec![json!("seek"), json!(secs), json!("relative")])
            .await;
    }

    async fn on_seek_input_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.app.seek_input = None;
            }
            KeyCode::Enter => {
                if let Some(input) = self.app.seek_input.take() {
                    if let Some(secs) = parse_time_input(&input) {
                        self.player_command(vec![json!("seek"), json!(secs), json!("absolute")])
                            .await;
                    }
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() || c == ':' => {
                if let Some(ref mut s) = self.app.seek_input {
                    s.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(ref mut s) = self.app.seek_input {
                    s.pop();
                }
            }
            _ => {}
        }
    }

    /// Hand the current stream off to a standalone mpv window. Embedded Kitty
    /// output makes the terminal cache every RGBA frame (multi-GB RSS); a normal
    /// mpv window renders to a GPU surface with no such cache. Resumes at the
    /// current position and returns the TUI to the episode list.
    async fn open_external(&mut self) {
        let (Some(source), Some((anime, episode))) =
            (self.current_source.clone(), self.playing.clone())
        else {
            return;
        };
        let pos = self.app.playback.map(|p| p.position).unwrap_or(0.0);

        // Stop embedded playback and wipe its Kitty output.
        self.save_current_progress();
        if let Some(player) = self.player.take() {
            player.stop().await;
        }
        self.app.playback = None;
        self.app.fullscreen = false;
        self.app.seek_input = None;
        self.needs_clear = true;
        self.app.view = View::Episodes;
        self.app.set_status("playing in external mpv window (best quality)");

        let (mpv, tx, prog_tx) =
            (self.mpv_path.clone(), self.tx.clone(), self.prog_tx.clone());
        tokio::spawn(async move {
            // Pre-resolved direct stream → mpv skips its yt-dlp pass and opens fast.
            let result = player::run_external(
                &mpv,
                &source.url,
                &source.headers,
                pos,
                crate::player::mpv::MpvTuning::high_quality(),
                prog_tx,
            )
            .await;
            let _ = tx.send(Msg::ExternalEnded { anime, episode, result });
        });
    }

    async fn toggle_fullscreen(&mut self) {
        self.app.fullscreen = !self.app.fullscreen;
        // Force an immediate chrome redraw before mpv respawns so the black
        // letterbox fill (or restored UI) is painted before the new kitty
        // frames arrive, not up to 900 ms later.
        self.last_tui_draw = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(2))
            .unwrap_or_else(std::time::Instant::now);
        if self.player.is_some() {
            self.on_resize().await;
        }
    }

    fn on_search_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char(c) => self.app.search_input.push(c),
            KeyCode::Backspace => {
                self.app.search_input.pop();
            }
            KeyCode::Esc => self.app.input_mode = false,
            KeyCode::Enter => {
                self.app.input_mode = false;
                let q = self.app.search_input.trim().to_string();
                if !q.is_empty() {
                    self.app.loading = true;
                    self.dispatch(Effect::Search(q));
                }
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, effect: Effect) {
        match effect {
            Effect::None => {}
            Effect::Search(query) => {
                let (provider, tx) = (self.provider.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let _ = tx.send(Msg::Results(provider.search(&query).await));
                });
            }
            Effect::LoadDetails(id) => {
                let (provider, tx) = (self.provider.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let _ = tx.send(Msg::Details(provider.details(&id).await));
                });
            }
            Effect::LoadFavourites => {
                // Favourites come from local DB — no network.
                match self.db.list_favourites(self.provider.name()) {
                    Ok(favs) => {
                        let _ = self.tx.send(Msg::Favourites(Ok(favs)));
                    }
                    Err(e) => {
                        let _ = self.tx.send(Msg::Favourites(Err(e)));
                    }
                }
            }
            Effect::LoadHistory => {
                match self.db.list_history(self.provider.name()) {
                    Ok(hist) => {
                        let _ = self.tx.send(Msg::History(Ok(hist)));
                    }
                    Err(e) => {
                        let _ = self.tx.send(Msg::History(Err(e)));
                    }
                }
            }
            Effect::Play(anime, episode) => {
                // Fast path: the episode was prefetched while hovering — play now.
                if let Some(sources) = self.source_cache.get(&episode.0).cloned() {
                    let _ = self.tx.send(Msg::Resolved {
                        anime,
                        episode,
                        result: Ok(sources),
                    });
                    return;
                }
                let (provider, tx) = (self.provider.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let result = resolve_and_prepare(&*provider, &anime, &episode).await;
                    let _ = tx.send(Msg::Resolved { anime, episode, result });
                });
            }
            Effect::SelectSource(i) => {
                if let Some(source) = self.pending_sources.get(i).cloned() {
                    // Use the (anime, episode) pair captured when the sources were
                    // resolved. Do NOT recompute from `episodes_state.selected()`:
                    // with the season-fold tree that is a ROW index (headers
                    // included), not an episode index, so it would map to the wrong
                    // episode — progress would save under the wrong id.
                    if let Some((anime, episode)) = self.playing.clone() {
                        let tx = self.tx.clone();
                        tokio::spawn(async move {
                            let mut s = source;
                            s.url = pre_resolve_url(&s.url).await;
                            let _ = tx.send(Msg::Resolved {
                                anime,
                                episode,
                                result: Ok(vec![s]),
                            });
                        });
                        self.app.view = View::Player;
                        self.app.loading = true;
                    }
                }
            }
            Effect::ToggleFavourite(id, title) => {
                let new_state = self
                    .db
                    .toggle_favourite(self.provider.name(), &id.0, &title)
                    .unwrap_or_else(|e| {
                        self.app.set_status(format!("favourite error: {e}"));
                        self.app.is_favourite
                    });
                self.app.is_favourite = new_state;
            }
        }
    }

    async fn on_message(&mut self, msg: Msg) {
        match msg {
            Msg::Results(Ok(results)) => {
                self.app.set_results(results);
                self.app.set_status(format!("provider: {} · backend: {:?}", self.provider.name(), self.backend));
            }
            Msg::Results(Err(e)) => self.fail(e),
            Msg::Details(Ok(details)) => {
                // Cache title for history lookups.
                let _ = self.db.cache_anime(
                    self.provider.name(),
                    &details.id.0,
                    &details.title,
                    None,
                );
                // Load resume positions and favourite flag.
                self.app.resume_positions = self
                    .db
                    .resume_positions_for_anime(self.provider.name(), &details.id.0)
                    .unwrap_or_default();
                self.app.is_favourite = self
                    .db
                    .is_favourite(self.provider.name(), &details.id.0)
                    .unwrap_or(false);
                // New anime: drop any prefetched sources from the previous one,
                // then queue the resume episode and the next one for warming.
                self.source_cache.clear();
                self.prefetch_target = None;
                self.prefetch_queue.clear();
                self.queue_resume_prefetch(&details);
                // Drop the previous poster and fetch this one's cover (Kitty only).
                self.current_poster = None;
                self.poster_dirty = false;
                if let Some(url) = details.poster_url.clone() {
                    self.fetch_poster(details.id.clone(), url);
                }
                self.app.set_details(details);
            }
            Msg::Details(Err(e)) => self.fail(e),
            Msg::Favourites(Ok(favs)) => self.app.set_results(favs),
            Msg::Favourites(Err(e)) => self.fail(e),
            Msg::History(Ok(hist)) => self.app.set_results(hist),
            Msg::History(Err(e)) => self.fail(e),
            Msg::Resolved { anime, episode, result } => match result {
                Ok(sources) if sources.is_empty() => {
                    self.fail(Error::Resolve("no sources returned".into()));
                    if self.app.view == View::Player {
                        self.app.view = View::Episodes;
                    }
                }
                Ok(sources) => {
                    if sources.len() == 1 {
                        self.pending_sources = sources.clone();
                        self.begin_playback(anime, episode, sources.into_iter().next().unwrap()).await;
                    } else {
                        // Multiple sources: show selection list; runner retains all of them.
                        let labels: Vec<String> = sources
                            .iter()
                            .map(|s| s.label.clone().unwrap_or_else(|| s.url.clone()))
                            .collect();
                        self.pending_sources = sources;
                        // Store the intended (anime, episode) pair so SelectSource can use it.
                        self.playing = Some((anime, episode));
                        self.app.loading = false;
                        self.app.set_sources(labels);
                    }
                }
                Err(e) => {
                    self.fail(e);
                    if self.app.view == View::Player {
                        self.app.view = View::Episodes;
                    }
                }
            },
            Msg::ExternalEnded { anime, episode, result } => {
                // Persist the final observed position so resume works, including
                // after an early quit. The throttled saves during playback may lag
                // by a few seconds, so save the last known position here too.
                if let Some(pb) = self.app.playback {
                    if pb.position > 1.0 {
                        let _ = self.db.save_progress(
                            self.provider.name(),
                            &anime.0,
                            &episode.0,
                            pb.position,
                            pb.duration,
                        );
                    }
                }
                self.app.playback = None;

                // Refresh the resume markers so progress shows immediately on
                // return, not only after re-opening the details page.
                self.reload_resume_positions();

                self.app.set_status(match result {
                    Ok(()) => "playback finished".into(),
                    Err(e) => format!("playback error: {e}"),
                });
                if self.playing.as_ref() == Some(&(anime, episode)) {
                    self.playing = None;
                }
                if self.app.view == View::Player {
                    self.app.view = View::Episodes;
                }
                self.app.loading = false;
            }
            Msg::Prefetched { episode, sources } => {
                if self.prefetch_inflight.as_deref() == Some(episode.0.as_str()) {
                    self.prefetch_inflight = None;
                }
                if !sources.is_empty() {
                    self.source_cache.insert(episode.0, sources);
                }
            }
            Msg::Poster { anime, png } => {
                // Only apply if it's still the anime being viewed.
                let current = self.app.details.as_ref().map(|d| d.id.0.as_str());
                if current == Some(anime.0.as_str()) {
                    if let Ok(png) = png {
                        self.current_poster = Some((anime, png));
                        self.poster_dirty = true;
                    }
                }
            }
        }
    }

    async fn begin_playback(&mut self, anime: AnimeId, episode: EpisodeId, source: PreparedSource) {
        // Never run two players at once.
        if let Some(p) = self.player.take() {
            p.stop().await;
        }

        let resume = self
            .db
            .resume_position(self.provider.name(), &anime.0, &episode.0)
            .ok()
            .flatten()
            .unwrap_or(0.0);

        self.app.loading = false;
        self.app.playback = Some(PlaybackState::default());
        self.playing = Some((anime.clone(), episode.clone()));
        self.current_source = Some(source.clone());
        self.last_saved_pos = resume;

        match self.backend {
            Backend::EmbeddedKitty => {
                let rect = current_video_rect(self.app.fullscreen);
                match EmbeddedPlayer::start(
                    &self.mpv_path,
                    &source.url,
                    &source.headers,
                    rect,
                    resume,
                    self.tuning,
                    self.prog_tx.clone(),
                )
                .await
                {
                    Ok(player) => {
                        self.player = Some(player);
                        self.app.set_status("playing (embedded)");
                    }
                    Err(e) => {
                        self.fail(e);
                        self.app.view = View::Episodes;
                    }
                }
            }
            Backend::ExternalMpv => {
                let (mpv, tx, prog_tx) =
                    (self.mpv_path.clone(), self.tx.clone(), self.prog_tx.clone());
                tokio::spawn(async move {
                    // Hand mpv the pre-resolved direct stream so it skips its own
                    // yt-dlp pass (the ~1-2 s launch delay); a generous buffer
                    // keeps HD smooth.
                    let result = player::run_external(
                        &mpv,
                        &source.url,
                        &source.headers,
                        resume,
                        crate::player::mpv::MpvTuning::high_quality(),
                        prog_tx,
                    )
                    .await;
                    let _ = tx.send(Msg::ExternalEnded { anime, episode, result });
                });
                self.app.set_status("playing (external mpv)");
            }
        }
    }

    fn on_progress(&mut self, update: ProgressUpdate) {
        self.app.playback = Some(PlaybackState {
            position: update.position,
            duration: update.duration,
            paused: update.paused,
            fps: update.fps,
        });
        let Some((anime, episode)) = self.playing.clone() else {
            return;
        };
        // Keep the in-memory resume marker live so the episode list shows the
        // current position immediately (independent of the DB save throttle and
        // of which stop path is taken). Only touch the map for the displayed anime.
        if update.position > 1.0
            && self.app.details.as_ref().map(|d| d.id.0.as_str()) == Some(anime.0.as_str())
        {
            self.app
                .resume_positions
                .insert(episode.0.clone(), update.position);
        }
        // Throttled, atomic DB persist.
        if (update.position - self.last_saved_pos).abs() >= self.save_interval {
            self.last_saved_pos = update.position;
            let _ = self.db.save_progress(
                self.provider.name(),
                &anime.0,
                &episode.0,
                update.position,
                update.duration,
            );
        }
    }

    async fn on_tick(&mut self) {
        // Detect embedded mpv exiting on its own (end of file / crash).
        if let Some(player) = &mut self.player {
            if player.finished() {
                self.end_playback("finished").await;
            }
        }
        // Warm the resolve/yt-dlp cache for the episode you're hovering, so
        // pressing Enter plays instantly instead of waiting ~2-3 s.
        self.maybe_prefetch();
        // NOTE: no periodic DELETE_ALL purge. The terminal image cache is bounded
        // by the terminal's own LRU eviction (and, for real relief, the external
        // player via the `o` key); a purge would only add a visible black flash.
        // /dev/shm is kept small by the GC task in EmbeddedPlayer.
    }

    /// Stop playback, save the final position, refresh the episode-list resume
    /// markers, and return to the episode list (wiping any embedded video).
    async fn end_playback(&mut self, status: &str) {
        self.save_current_progress();
        self.reload_resume_positions();
        if let Some(player) = self.player.take() {
            player.stop().await;
        } else {
            // External window backend has no in-process handle; ask it to quit
            // over its IPC socket (no-op if nothing is playing there).
            crate::player::embedded::quit(&crate::player::external_socket_path()).await;
        }
        self.app.playback = None;
        self.app.fullscreen = false;
        self.app.seek_input = None;
        self.playing = None;
        self.current_source = None;
        self.needs_clear = true;
        if self.app.view == View::Player {
            self.app.view = View::Episodes;
        }
        self.app.set_status(status);
    }

    /// Queue the resume episode (most recently watched) and the one after it for
    /// proactive prefetch, so "continue watching" plays instantly.
    fn queue_resume_prefetch(&mut self, details: &AnimeDetails) {
        let last = self
            .db
            .last_watched_episode(self.provider.name(), &details.id.0)
            .ok()
            .flatten();
        // Start at the resume episode if known, otherwise the first episode.
        let start = last
            .and_then(|ep| details.episodes.iter().position(|e| e.id.0 == ep))
            .unwrap_or(0);
        for idx in [start, start + 1] {
            if let Some(e) = details.episodes.get(idx) {
                self.prefetch_queue
                    .push_back((details.id.clone(), e.id.clone()));
            }
        }
    }

    /// Spawn a background task to fetch/decode/cache an anime's poster (Kitty
    /// terminals only). Reports `Msg::Poster` when the PNG is ready.
    fn fetch_poster(&self, anime: AnimeId, url: String) {
        if !crate::player::kitty::probe_support() {
            return; // no graphics support → details page stays text-only
        }
        let cache_path = self.poster_cache.path_for(&format!("{}.png", anime.0));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let png = load_or_fetch_poster(&cache_path, &url).await;
            let _ = tx.send(Msg::Poster { anime, png });
        });
    }

    /// Spawn a background resolve+pre-resolve for one episode, caching the result.
    fn start_prefetch(&mut self, anime: AnimeId, episode: EpisodeId) {
        self.prefetch_inflight = Some(episode.0.clone());
        let (provider, tx) = (self.provider.clone(), self.tx.clone());
        tokio::spawn(async move {
            let sources = resolve_and_prepare(&*provider, &anime, &episode)
                .await
                .unwrap_or_default();
            let _ = tx.send(Msg::Prefetched { episode, sources });
        });
    }

    /// Warm the source cache: first the proactive queue (resume + next), then the
    /// currently-hovered episode. Limited to one in-flight resolve so scrolling
    /// doesn't spawn a yt-dlp per episode.
    fn maybe_prefetch(&mut self) {
        if self.player.is_some() || self.playing.is_some() || self.prefetch_inflight.is_some() {
            return;
        }
        // Proactive queue (resume episode + next), warmed even from the details page.
        while let Some((anime, episode)) = self.prefetch_queue.pop_front() {
            if self.source_cache.contains_key(&episode.0) {
                continue; // already warm
            }
            self.start_prefetch(anime, episode);
            return;
        }
        // Hover-based prefetch of the highlighted episode (Episodes view only).
        if self.app.view != View::Episodes {
            return;
        }
        let Some(idx) = self.app.selected_episode_index() else {
            return; // a season header (or nothing) is selected
        };
        let Some((anime, episode)) = self
            .app
            .details
            .as_ref()
            .and_then(|d| d.episodes.get(idx).map(|e| (d.id.clone(), e.id.clone())))
        else {
            return;
        };
        let key = episode.0.clone();
        if self.source_cache.contains_key(&key) {
            return; // already warm
        }
        // Debounce: require the selection to have rested on this episode a moment.
        let now = std::time::Instant::now();
        match &self.prefetch_target {
            Some((k, since))
                if *k == key && now.duration_since(*since) >= std::time::Duration::from_millis(400) => {}
            Some((k, _)) if *k == key => return, // resting, but not long enough yet
            _ => {
                self.prefetch_target = Some((key, now));
                return;
            }
        }
        self.start_prefetch(anime, episode);
    }

    /// Reload the resume-position markers shown in the episode list for the
    /// currently loaded anime, so progress appears immediately after playback
    /// without needing to re-open the details page.
    fn reload_resume_positions(&mut self) {
        if let Some(id) = self.app.details.as_ref().map(|d| d.id.0.clone()) {
            self.app.resume_positions = self
                .db
                .resume_positions_for_anime(self.provider.name(), &id)
                .unwrap_or_default();
        }
    }

    async fn change_episode(&mut self, delta: isize) {
        // Determine the current episode index from what's playing (robust even if
        // a season header is the selected row); fall back to the selected episode.
        let fallback = self.app.selected_episode_index().unwrap_or(0);
        let Some(details) = &self.app.details else { return };
        let len = details.episodes.len();
        if len == 0 {
            return;
        }
        let current = self
            .playing
            .as_ref()
            .and_then(|(_, ep)| details.episodes.iter().position(|e| &e.id == ep))
            .unwrap_or(fallback) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        let anime = details.id.clone();
        let episode = details.episodes[next].id.clone();

        // Expand the target episode's season and move the cursor onto it, so the
        // episode list reflects what's now playing (and n/p keep working).
        self.app.select_episode(next);
        self.end_playback("switching episode").await;
        self.app.view = View::Player;
        self.app.loading = true;
        self.dispatch(Effect::Play(anime, episode));
    }

    /// On resize, respawn embedded mpv aligned to the new rectangle, resuming at
    /// the current position. External mpv manages its own window.
    async fn on_resize(&mut self) {
        if self.player.is_none() {
            return;
        }
        let (Some(source), Some((anime, episode))) =
            (self.current_source.clone(), self.playing.clone())
        else {
            return;
        };
        let pos = self.app.playback.map(|p| p.position).unwrap_or(0.0);

        if let Some(player) = self.player.take() {
            player.stop().await;
        }
        // Wipe old placements before the new instance paints.
        let mut out = std::io::stdout();
        let _ = out.write_all(DELETE_ALL_IMAGES.as_bytes());
        let _ = out.flush();

        let rect = current_video_rect(self.app.fullscreen);
        match EmbeddedPlayer::start(
            &self.mpv_path,
            &source.url,
            &source.headers,
            rect,
            pos,
            self.tuning,
            self.prog_tx.clone(),
        )
        .await
        {
            Ok(player) => {
                self.player = Some(player);
                self.playing = Some((anime, episode));
                self.current_source = Some(source);
            }
            Err(e) => self.end_playback(&format!("resize restart failed: {e}")).await,
        }
    }

    fn save_current_progress(&self) {
        if let (Some((anime, episode)), Some(pb)) = (&self.playing, self.app.playback) {
            let _ = self.db.save_progress(
                self.provider.name(),
                &anime.0,
                &episode.0,
                pb.position,
                pb.duration,
            );
        }
    }

    fn fail(&mut self, e: Error) {
        self.app.loading = false;
        self.app.set_status(format!("error: {e}"));
    }
}

/// Current reserved video rectangle, from the live terminal size + fullscreen flag.
fn current_video_rect(fullscreen: bool) -> crate::player::kitty::CellRect {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    ui::video_rect(Rect::new(0, 0, cols, rows), fullscreen)
}

/// Parse a user-typed time string into seconds.
/// Accepts: "90" (seconds), "1:30" (mm:ss), "1:02:30" (hh:mm:ss).
fn parse_time_input(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [secs] => secs.trim().parse::<f64>().ok(),
        [mins, secs] => {
            let m = mins.trim().parse::<u64>().ok()?;
            let s = secs.trim().parse::<f64>().ok()?;
            Some(m as f64 * 60.0 + s)
        }
        [hours, mins, secs] => {
            let h = hours.trim().parse::<u64>().ok()?;
            let m = mins.trim().parse::<u64>().ok()?;
            let s = secs.trim().parse::<f64>().ok()?;
            Some(h as f64 * 3600.0 + m as f64 * 60.0 + s)
        }
        _ => None,
    }
}

/// Try to resolve an embed URL to a direct stream URL via yt-dlp before
/// handing it to mpv. This eliminates the yt-dlp subprocess delay inside mpv,
/// so the first frame appears as soon as the network buffer is filled.
/// Falls back to the original URL on any error.
async fn pre_resolve_url(url: &str) -> String {
    if url.starts_with("file://") || is_direct_stream(url) {
        // Already a playable stream (local file or direct HLS/MP4) — skip yt-dlp
        // entirely; spawning it here would just add ~1-2 s of latency.
        return url.to_string();
    }
    let Ok(out) = tokio::process::Command::new("yt-dlp")
        .args(["-g", "--no-playlist", "--quiet", "--no-warnings", url])
        .output()
        .await
    else {
        return url.to_string();
    };
    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        // yt-dlp can return multiple lines (audio + video for adaptive streams);
        // take the first line which is always the video stream.
        if let Some(line) = stdout.lines().next() {
            let direct = line.trim();
            if direct.starts_with("http://") || direct.starts_with("https://") {
                return direct.to_string();
            }
        }
    }
    url.to_string()
}

/// True if `url` already points at a stream mpv can open directly, so yt-dlp
/// resolution can be skipped. Conservative: only obvious HLS/MP4 media URLs.
fn is_direct_stream(url: &str) -> bool {
    // Ignore any query string / fragment when checking the path extension.
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let path = path.to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".mp4") || path.ends_with(".mkv")
}

/// Resolve an episode to a list of validated playable sources. http/https URLs
/// pass the scheme allowlist; `file://` is accepted only as a local/dev
/// affordance (used by the mock provider).
async fn resolve_sources(
    provider: &dyn Provider,
    anime: &AnimeId,
    episode: &EpisodeId,
) -> Result<Vec<PreparedSource>> {
    let sources = provider.resolve(anime, episode).await?;
    let prepared: Vec<PreparedSource> = sources
        .into_iter()
        .filter_map(|s| {
            let url = if s.url.starts_with("file://") {
                Some(s.url.clone())
            } else {
                crate::resolver::validate_stream_url(&s.url).ok()
            };
            url.map(|u| PreparedSource {
                url: u,
                headers: s.http_headers,
                label: s.label,
            })
        })
        .collect();
    if prepared.is_empty() {
        return Err(Error::Resolve("no valid sources returned".into()));
    }
    Ok(prepared)
}

/// Load a poster PNG from the on-disk cache, else fetch the URL, decode, downscale
/// to a 2:3 cover (Kitty rescales into the cell box on display), re-encode PNG, and
/// cache it. Returns the PNG bytes ready for `kitty::transmit_png`.
async fn load_or_fetch_poster(cache_path: &std::path::Path, url: &str) -> Result<Vec<u8>> {
    if let Ok(bytes) = tokio::fs::read(cache_path).await {
        if !bytes.is_empty() {
            return Ok(bytes);
        }
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(Error::InvalidUrl(url.to_string()));
    }
    let raw = reqwest::get(url)
        .await
        .map_err(|e| Error::Network(format!("poster fetch: {e}")))?
        .bytes()
        .await
        .map_err(|e| Error::Network(format!("poster body: {e}")))?;

    // Decode + resize on a blocking thread (image work is CPU-bound).
    let png = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let img = image::load_from_memory(&raw)
            .map_err(|e| Error::Resolve(format!("poster decode: {e}")))?;
        let cover = img.resize_to_fill(300, 450, image::imageops::FilterType::Triangle);
        let mut out = std::io::Cursor::new(Vec::new());
        cover
            .write_to(&mut out, image::ImageFormat::Png)
            .map_err(|e| Error::Resolve(format!("poster encode: {e}")))?;
        Ok(out.into_inner())
    })
    .await
    .map_err(|e| Error::Resolve(format!("poster task: {e}")))??;

    let _ = tokio::fs::write(cache_path, &png).await;
    Ok(png)
}

/// Resolve an episode AND pre-resolve every source's direct stream URL (in
/// parallel) via yt-dlp, so both single-source play and source selection are
/// instant afterwards. Used by the direct-play path and by prefetch.
async fn resolve_and_prepare(
    provider: &dyn Provider,
    anime: &AnimeId,
    episode: &EpisodeId,
) -> Result<Vec<PreparedSource>> {
    let mut sources = resolve_sources(provider, anime, episode).await?;
    let resolved = futures::future::join_all(
        sources
            .iter()
            .map(|s| {
                let url = s.url.clone();
                async move { pre_resolve_url(&url).await }
            })
            .collect::<Vec<_>>(),
    )
    .await;
    for (s, u) in sources.iter_mut().zip(resolved) {
        s.url = u;
    }
    Ok(sources)
}
