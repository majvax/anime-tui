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
use crate::models::{AnimeDetails, AnimeId, AnimeSummary, CatalogPage, EpisodeId};
use crate::player::embedded::{EmbeddedPlayer, ProgressUpdate};
use crate::player::kitty::{CellRect, DELETE_ALL_IMAGES};
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
    /// First page of a fresh catalogue query (replaces the list).
    Results(Result<CatalogPage>),
    /// A subsequent catalogue page (appended for infinite scroll).
    MoreResults(Result<CatalogPage>),
    Details(Result<AnimeDetails>),
    Favourites(Result<Vec<AnimeSummary>>),
    History(Result<Vec<AnimeSummary>>),
    Resolved {
        anime: AnimeId,
        episode: EpisodeId,
        result: Result<Vec<PreparedSource>>,
        /// True when the user asked to pick a source (show the picker); false plays
        /// the configured default source directly.
        choose: bool,
    },
    /// External-backend playback finished (embedded finish is detected via tick).
    ExternalEnded {
        anime: AnimeId,
        episode: EpisodeId,
        result: Result<()>,
    },
    /// Play an ordered set of fallback candidates for an episode (picker choice).
    PlayQueue {
        anime: AnimeId,
        episode: EpisodeId,
        queue: Vec<PreparedSource>,
    },
    /// Background prefetch of an episode's sources completed (empty on failure).
    Prefetched {
        episode: EpisodeId,
        sources: Vec<PreparedSource>,
    },
    /// Fetched + decoded poster image, ready to resize to the display box.
    Poster {
        anime: AnimeId,
        image: Result<image::DynamicImage>,
    },
    /// Fetched + decoded browse-row thumbnail, pre-sized to a small PNG.
    Thumb {
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

    /// Ordered playback candidates for the episode being played, and the index of
    /// the next one to try. When a source fails to start (mpv exit ≠ 0 before any
    /// progress), playback falls back to the next candidate — so a dead/blocked
    /// host (e.g. voe) transparently yields to a working one.
    playback_queue: Vec<PreparedSource>,
    playback_attempt: usize,

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
    /// Preferred source label for direct (Enter) playback, e.g. "vidmoly (VF)".
    default_source: String,
    /// On-disk cache of raw poster source bytes.
    poster_cache: crate::cache::Cache,
    /// The current poster's decoded image, resized to the display box at transmit.
    current_poster: Option<(AnimeId, image::DynamicImage)>,
    /// Set when the poster must be (re)painted into its reserved rect.
    poster_dirty: bool,
    /// Whether the poster image is currently placed on screen (for clean removal).
    poster_shown: bool,
    /// The rect the poster was last transmitted into, so a view switch (left↔right)
    /// or resize re-places it even when the image content is unchanged.
    last_poster_rect: Option<CellRect>,
    /// Anime id whose (details) poster fetch is in flight (at most one at a time).
    poster_inflight: Option<String>,
    /// The cover URL last fetched for the current details anime. Lets us start the
    /// poster from the browse summary URL immediately and skip the redundant
    /// higher-res `Msg::Details` fetch when it's the same image.
    last_poster_url: Option<String>,

    /// Shared HTTP client for poster/thumbnail fetches (connection pooling — the
    /// per-call `reqwest::get` was the ~2 s cost while browsing).
    http: reqwest::Client,
    /// Ready-to-transmit small PNG thumbnails keyed by anime id (browse rows).
    thumb_cache: std::collections::HashMap<String, Vec<u8>>,
    /// Thumbnail fetches currently in flight (bounded concurrency).
    thumb_inflight: std::collections::HashSet<String>,
    /// Covers transmitted to the terminal (once each): anime id → Kitty image id.
    thumb_img: std::collections::HashMap<String, u32>,
    /// Next image id to assign (counts up from `THUMB_ID_BASE`).
    thumb_next_id: u32,
    /// Current placement on each visible row slot: slot → image id (placement id is
    /// `slot + 1`). Scrolling only moves placements, never re-sends image data.
    placed_slots: std::collections::HashMap<u16, u32>,

    /// Set when the video surface must be wiped and the TUI fully repainted
    /// (playback ended / resize). Acted on in `run` where the terminal lives.
    needs_clear: bool,

    /// Last time the TUI chrome was redrawn during embedded playback. Used to
    /// throttle ratatui flushes so they don't compete with mpv's Kitty output.
    last_tui_draw: std::time::Instant,

    /// mpv read-ahead buffer caps (its own RAM, not the terminal image cache).
    tuning: crate::player::mpv::MpvTuning,
    /// Path to the generated mpv input.conf that gives the external window our
    /// keybinds (None if it couldn't be written → mpv defaults still apply).
    input_conf_path: Option<String>,
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

        // Write the mpv input.conf that mirrors our keybinds for the external
        // window. Best-effort: on failure the window just uses mpv's defaults.
        let input_conf_path = {
            let dir = config.cache_dir().unwrap_or_else(|_| std::env::temp_dir());
            let path = dir.join("mpv-input.conf");
            let conf = crate::player::mpv::external_input_conf(config.playback.skip_intro_secs);
            std::fs::write(&path, conf)
                .ok()
                .map(|_| path.to_string_lossy().into_owned())
        };

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
            playback_queue: Vec::new(),
            playback_attempt: 0,
            source_cache: std::collections::HashMap::new(),
            prefetch_inflight: None,
            prefetch_target: None,
            prefetch_queue: std::collections::VecDeque::new(),
            skip_intro_secs: config.playback.skip_intro_secs,
            default_source: config.playback.default_source.clone(),
            poster_cache,
            current_poster: None,
            poster_dirty: false,
            poster_shown: false,
            last_poster_rect: None,
            poster_inflight: None,
            last_poster_url: None,
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            thumb_cache: std::collections::HashMap::new(),
            thumb_inflight: std::collections::HashSet::new(),
            thumb_img: std::collections::HashMap::new(),
            thumb_next_id: crate::player::kitty::THUMB_ID_BASE,
            placed_slots: std::collections::HashMap::new(),
            needs_clear: false,
            last_tui_draw: std::time::Instant::now(),
            tuning: crate::player::mpv::MpvTuning {
                max_buffer_mib: config.playback.max_buffer_mib,
                readahead_secs: config.playback.readahead_secs,
            },
            input_conf_path,
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
                // DELETE_ALL_IMAGES freed every thumbnail image + placement; forget them.
                self.placed_slots.clear();
                self.thumb_img.clear();
                self.thumb_next_id = crate::player::kitty::THUMB_ID_BASE;
            }

            // Keep the browse scroll offset in sync before drawing so the row
            // thumbnails line up with what ratatui renders.
            self.update_list_offset();
            // Infinite scroll: fetch the next catalogue page as the selection nears
            // the end of what's loaded.
            self.maybe_load_more();

            // During embedded playback, mpv writes Kitty frames to the same
            // stdout as ratatui. Throttle TUI chrome redraws to once per second
            // so they don't compete with video frames and cause stutter.
            let player_active = self.player.is_some();
            let since_last = self.last_tui_draw.elapsed();
            if !player_active || since_last >= std::time::Duration::from_millis(900) {
                guard.terminal.draw(|f| ui::render(f, &self.app))?;
                self.last_tui_draw = std::time::Instant::now();
            }

            // After the TUI draw (so ratatui doesn't clobber them): the details
            // poster and the per-row browse thumbnails.
            self.render_poster();
            self.place_thumbnails();
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

    /// Paint the details-page poster into its reserved (left) rect, resizing the
    /// decoded image to the exact box for crispness, or remove it when off the
    /// details page. Called after the TUI draw so it isn't overpainted.
    fn render_poster(&mut self) {
        use std::io::Write as _;
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let area = Rect::new(0, 0, cols, rows);
        let rect = if self.app.view == View::Details {
            ui::details_poster_rect(area)
        } else {
            CellRect { left: 0, top: 0, cols: 0, rows: 0, pixel_width: None, pixel_height: None }
        };

        if !rect.is_empty() {
            if let Some((_, img)) = &self.current_poster {
                // Re-place on: content change, first show, or a moved/resized rect
                // (e.g. switching between the details left pane and browse right pane).
                let moved = self.last_poster_rect != Some(rect);
                if self.poster_dirty || !self.poster_shown || moved {
                    // Resize to the box's exact pixels so the terminal doesn't
                    // up/down-scale it (Lanczos3 keeps it sharp).
                    let pw = rect.pixel_width.map(u32::from).unwrap_or(rect.cols as u32 * 8).max(1);
                    let ph = rect.pixel_height.map(u32::from).unwrap_or(rect.rows as u32 * 16).max(1);
                    let sized = img.resize_to_fill(pw, ph, image::imageops::FilterType::Lanczos3);
                    let mut png = std::io::Cursor::new(Vec::new());
                    if sized.write_to(&mut png, image::ImageFormat::Png).is_ok() {
                        let mut out = std::io::stdout();
                        let _ = out.write_all(crate::player::kitty::DELETE_POSTER.as_bytes());
                        let _ = out.write_all(
                            crate::player::kitty::transmit_png(png.get_ref(), rect).as_bytes(),
                        );
                        let _ = out.flush();
                        self.poster_dirty = false;
                        self.poster_shown = true;
                        self.last_poster_rect = Some(rect);
                    }
                }
                return;
            }
        }
        // No poster to show in this view (or none loaded): remove any placement.
        if self.poster_shown {
            let mut out = std::io::stdout();
            let _ = out.write_all(crate::player::kitty::DELETE_POSTER.as_bytes());
            let _ = out.flush();
            self.poster_shown = false;
            self.last_poster_rect = None;
        }
    }

    /// True when the current view is one of the scrollable browse lists.
    fn in_browse_view(&self) -> bool {
        matches!(
            self.app.view,
            View::Home | View::Search | View::Favourites | View::History
        )
    }

    /// Keep the selected browse row within the visible window by adjusting
    /// `list_offset` (only scrolls when the selection leaves the window). Must run
    /// before the draw so `render_list` renders at this offset.
    fn update_list_offset(&mut self) {
        if !self.in_browse_view() {
            return;
        }
        let (_, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let body = ui::browse_body(Rect::new(0, 0, 1, rows));
        let inner_h = body.height.saturating_sub(2);
        let visible = (inner_h / ui::ROW_H).max(1) as usize;
        let len = self.app.results.len();
        if len == 0 {
            self.app.list_offset = 0;
            return;
        }
        let sel = self.app.results_state.selected().unwrap_or(0);
        self.app.list_offset = keep_in_view(self.app.list_offset, sel, visible, len);
    }

    /// Fetch the next catalogue page when the selection nears the bottom of the
    /// loaded results (infinite scroll). Catalogue only (Home/Search) — favourites
    /// and history are single local-DB lists. Skipped while a quick-filter is
    /// active (the user is narrowing, not paging) or a page is already in flight.
    fn maybe_load_more(&mut self) {
        if !matches!(self.app.view, View::Home | View::Search) {
            return;
        }
        if !self.app.filter.is_empty()
            || self.app.loading_more
            || self.app.page >= self.app.total_pages
        {
            return;
        }
        let (_, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let body = ui::browse_body(Rect::new(0, 0, 1, rows));
        let inner_h = body.height.saturating_sub(2);
        let visible = (inner_h / ui::ROW_H).max(1) as usize;
        let len = self.app.results.len();
        let sel = self.app.results_state.selected().unwrap_or(0);
        // Trigger within one screen of the end.
        if sel + visible < len {
            return;
        }
        self.app.loading_more = true;
        let (query, next, sort) = (
            self.app.query.clone(),
            self.app.page + 1,
            self.app.sort.param(),
        );
        let (provider, tx) = (self.provider.clone(), self.tx.clone());
        tokio::spawn(async move {
            let _ = tx.send(Msg::MoreResults(provider.search_page(&query, next, sort).await));
        });
    }

    /// Concurrent thumbnail fetches allowed (kept moderate to be polite but snappy).
    const THUMB_FETCH_CAP: usize = 8;

    /// Place a cover thumbnail on each visible browse row (Kitty terminals) and
    /// eagerly warm the rest, or remove them when off a browse view. Incremental:
    /// only slots whose content changed are (re)transmitted, so a newly-fetched
    /// thumbnail pops in instantly and idle frames do no work. Runs after the draw.
    fn place_thumbnails(&mut self) {
        let browse = self.in_browse_view() && crate::player::kitty::probe_support();
        if !browse {
            self.clear_thumbnails();
            return;
        }
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let body = ui::browse_body(Rect::new(0, 0, cols, rows));
        let (ix, iy) = (body.x + 1, body.y + 1);
        let (iw, ih) = (body.width.saturating_sub(2), body.height.saturating_sub(2));
        if iw < ui::THUMB_COLS + 2 || ih < ui::ROW_H {
            self.clear_thumbnails();
            return;
        }
        let visible = (ih / ui::ROW_H) as usize;
        let offset = self.app.list_offset;
        let len = self.app.results.len();

        // Eagerly fetch missing thumbnails — visible rows first, then the rest — so
        // scrolling finds them cached (instant). Bounded concurrency.
        let mut to_fetch: Vec<(String, String)> = Vec::new();
        let order = (offset..(offset + visible).min(len)).chain(0..len);
        for idx in order {
            if self.thumb_inflight.len() + to_fetch.len() >= Self::THUMB_FETCH_CAP {
                break;
            }
            let a = &self.app.results[idx];
            let id = &a.id.0;
            if !self.thumb_cache.contains_key(id)
                && !self.thumb_inflight.contains(id)
                && !to_fetch.iter().any(|(i, _)| i == id)
            {
                if let Some(url) = &a.poster_url {
                    to_fetch.push((id.clone(), url.clone()));
                }
            }
        }
        for (id, url) in to_fetch {
            self.fetch_thumb(AnimeId(id), url);
        }

        let mut out = std::io::stdout();
        let mut wrote = false;

        // 1) Transmit (once) any visible cover we have cached but haven't sent yet.
        let mut to_send: Vec<String> = Vec::new();
        for i in 0..visible {
            if let Some(a) = self.app.results.get(offset + i) {
                if self.thumb_cache.contains_key(&a.id.0) && !self.thumb_img.contains_key(&a.id.0) {
                    to_send.push(a.id.0.clone());
                }
            }
        }
        for anime in to_send {
            if let Some(png) = self.thumb_cache.get(&anime) {
                let id = self.thumb_next_id;
                self.thumb_next_id += 1;
                let _ = out.write_all(crate::player::kitty::transmit_data(png, id).as_bytes());
                self.thumb_img.insert(anime, id);
                wrote = true;
            }
        }

        // 2) Desired placement per slot: slot → image id, for visible transmitted covers.
        let want: std::collections::HashMap<u16, u32> = (0..visible as u16)
            .filter_map(|slot| {
                let a = self.app.results.get(offset + slot as usize)?;
                self.thumb_img.get(&a.id.0).map(|id| (slot, *id))
            })
            .collect();

        // 3) Diff placements (cheap — no image data): scrolling just moves these.
        let stale: Vec<(u16, u32)> = self
            .placed_slots
            .iter()
            .filter(|(slot, img)| want.get(slot) != Some(img))
            .map(|(slot, img)| (*slot, *img))
            .collect();
        for (slot, img) in stale {
            let _ = out.write_all(crate::player::kitty::delete_placement(img, (slot + 1) as u32).as_bytes());
            self.placed_slots.remove(&slot);
            wrote = true;
        }
        for (slot, img) in &want {
            if self.placed_slots.get(slot) == Some(img) {
                continue; // already placed here
            }
            let rect = CellRect {
                left: ix,
                top: iy + slot * ui::ROW_H,
                cols: ui::THUMB_COLS,
                rows: ui::ROW_H,
                pixel_width: None,
                pixel_height: None,
            };
            let _ = out
                .write_all(crate::player::kitty::place(*img, (slot + 1) as u32, rect).as_bytes());
            self.placed_slots.insert(*slot, *img);
            wrote = true;
        }
        if wrote {
            let _ = out.flush();
        }
    }

    /// Remove the row-thumbnail PLACEMENTS but keep the transmitted image data, so
    /// returning to the list re-shows them instantly.
    fn clear_thumbnails(&mut self) {
        if self.placed_slots.is_empty() {
            return;
        }
        let mut out = std::io::stdout();
        for (slot, img) in self.placed_slots.drain() {
            let _ = out.write_all(crate::player::kitty::delete_placement(img, (slot + 1) as u32).as_bytes());
        }
        let _ = out.flush();
    }

    /// Free every transmitted thumbnail image (data + placements) — called when the
    /// browse result set changes, so terminal image memory stays bounded to a page.
    fn free_thumb_images(&mut self) {
        if self.thumb_img.is_empty() {
            self.placed_slots.clear();
            return;
        }
        let mut out = std::io::stdout();
        for id in self.thumb_img.drain().map(|(_, id)| id) {
            let _ = out.write_all(crate::player::kitty::free_image(id).as_bytes());
        }
        let _ = out.flush();
        self.placed_slots.clear();
        self.thumb_next_id = crate::player::kitty::THUMB_ID_BASE;
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

        let (mpv, tx, prog_tx, conf) = (
            self.mpv_path.clone(),
            self.tx.clone(),
            self.prog_tx.clone(),
            self.input_conf_path.clone(),
        );
        tokio::spawn(async move {
            // Pre-resolved direct stream → mpv skips its yt-dlp pass and opens fast.
            let result = player::run_external(
                &mpv,
                &source.url,
                &source.headers,
                pos,
                crate::player::mpv::MpvTuning::high_quality(),
                conf,
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
        // Quick-filter capture (client-side, live) vs server-search capture.
        if self.app.filtering {
            match code {
                KeyCode::Char(c) => self.app.push_filter_char(c),
                KeyCode::Backspace => self.app.pop_filter_char(),
                KeyCode::Enter => {
                    // Commit: keep the filter applied, leave capture.
                    self.app.filtering = false;
                    self.app.input_mode = false;
                }
                KeyCode::Esc => {
                    // Cancel: clear the filter, leave capture.
                    self.app.clear_filter();
                    self.app.filtering = false;
                    self.app.input_mode = false;
                }
                _ => {}
            }
            return;
        }
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
                // Record the active query synchronously so paging/sort use it.
                self.app.query = query.clone();
                let sort = self.app.sort.param();
                let (provider, tx) = (self.provider.clone(), self.tx.clone());
                tokio::spawn(async move {
                    let _ = tx.send(Msg::Results(provider.search_page(&query, 1, sort).await));
                });
            }
            Effect::LoadDetails(id) => {
                // Leaving the browse list for Details: remove the row thumbnails now
                // (not on the next frame) so none linger over the details page, and
                // drop the previous poster so a stale cover isn't shown.
                self.clear_thumbnails();
                self.current_poster = None;
                self.poster_dirty = false;
                // Fast path: start fetching the cover from the browse summary's URL
                // immediately, in parallel with the details HTML round-trip, so the
                // poster isn't gated on that request. The details cover (higher-res)
                // upgrades it later if it's a different URL.
                self.last_poster_url = self
                    .app
                    .results
                    .iter()
                    .find(|a| a.id == id)
                    .and_then(|a| a.poster_url.clone());
                if let Some(url) = self.last_poster_url.clone() {
                    self.fetch_poster(id.clone(), url);
                }
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
            Effect::Play(anime, episode) => self.dispatch_resolve(anime, episode, false),
            Effect::PlayChoose(anime, episode) => self.dispatch_resolve(anime, episode, true),
            Effect::SelectSource(i) => {
                // Use the (anime, episode) pair captured when the sources were
                // resolved. Do NOT recompute from `episodes_state.selected()`:
                // with the season-fold tree that is a ROW index (headers included),
                // not an episode index, so it would map to the wrong episode.
                if i < self.pending_sources.len() {
                    if let Some((anime, episode)) = self.playing.clone() {
                        // Play the chosen source first, then fall back through the
                        // rest (already resolved) if it fails to start.
                        let mut queue = Vec::with_capacity(self.pending_sources.len());
                        queue.push(self.pending_sources[i].clone());
                        for (j, s) in self.pending_sources.iter().enumerate() {
                            if j != i {
                                queue.push(s.clone());
                            }
                        }
                        self.app.view = View::Player;
                        self.app.loading = true;
                        let _ = self.tx.send(Msg::PlayQueue { anime, episode, queue });
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

    /// Resolve an episode's sources (prefetched cache first, else via yt-dlp) and
    /// report them back as `Msg::Resolved`. `choose` requests the source picker;
    /// otherwise the configured default source is played directly.
    fn dispatch_resolve(&mut self, anime: AnimeId, episode: EpisodeId, choose: bool) {
        // Fast path: the episode was prefetched while hovering — play now.
        if let Some(sources) = self.source_cache.get(&episode.0).cloned() {
            let _ = self.tx.send(Msg::Resolved { anime, episode, result: Ok(sources), choose });
            return;
        }
        let (provider, tx) = (self.provider.clone(), self.tx.clone());
        tokio::spawn(async move {
            let result = resolve_and_prepare(&*provider, &anime, &episode).await;
            let _ = tx.send(Msg::Resolved { anime, episode, result, choose });
        });
    }

    async fn on_message(&mut self, msg: Msg) {
        match msg {
            Msg::Results(Ok(page)) => {
                self.free_thumb_images(); // fresh result set: bound terminal image memory
                self.app.set_page(page, false);
                self.app.set_status(format!("provider: {} · backend: {:?}", self.provider.name(), self.backend));
            }
            Msg::Results(Err(e)) => self.fail(e),
            // A further page arrived — append without disturbing existing covers.
            Msg::MoreResults(Ok(page)) => self.app.set_page(page, true),
            Msg::MoreResults(Err(e)) => {
                // Non-fatal: keep the list we have, just surface the reason.
                self.app.loading_more = false;
                self.app.set_status(format!("load more failed: {e}"));
            }
            Msg::Details(Ok(details)) => {
                // Cache title + cover so history/favourites can show them later.
                let _ = self.db.cache_anime(
                    self.provider.name(),
                    &details.id.0,
                    &details.title,
                    None,
                    details.poster_url.as_deref(),
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
                // The poster was already kicked off from the summary URL in
                // LoadDetails; upgrade to the details cover (higher-res extraLarge)
                // only when it's a different URL — no reset, so the fast cover isn't
                // blanked, and no redundant fetch when the URLs match.
                if let Some(url) = details.poster_url.clone() {
                    if self.last_poster_url.as_deref() != Some(url.as_str()) {
                        self.last_poster_url = Some(url.clone());
                        self.fetch_poster(details.id.clone(), url);
                    }
                }
                self.app.set_details(details);
            }
            Msg::Details(Err(e)) => self.fail(e),
            Msg::Favourites(Ok(favs)) => {
                self.free_thumb_images();
                self.app.set_results(favs);
            }
            Msg::Favourites(Err(e)) => self.fail(e),
            Msg::History(Ok(hist)) => {
                self.free_thumb_images();
                self.app.set_results(hist);
            }
            Msg::History(Err(e)) => self.fail(e),
            Msg::Resolved { anime, episode, result, choose } => match result {
                Ok(sources) if sources.is_empty() => {
                    self.fail(Error::Resolve("no sources returned".into()));
                    if self.app.view == View::Player {
                        self.app.view = View::Episodes;
                    }
                }
                // Explicit "choose source" with more than one option → show the picker.
                Ok(sources) if choose && sources.len() > 1 => {
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
                // Default: play the preferred source directly, falling back through the
                // rest (reliability order) if it fails to start.
                Ok(sources) => {
                    self.pending_sources = sources.clone();
                    let start = pick_default_index(&sources, &self.default_source);
                    let mut queue = Vec::with_capacity(sources.len());
                    queue.push(sources[start].clone());
                    for (j, s) in sources.into_iter().enumerate() {
                        if j != start {
                            queue.push(s);
                        }
                    }
                    self.app.view = View::Player;
                    self.start_playback_queue(anime, episode, queue).await;
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
                let pos = self.app.playback.map(|pb| pb.position).unwrap_or(0.0);
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

                // Early failure (mpv errored before any real progress) with more
                // candidates left → fall back to the next source rather than giving up.
                let still_current = self.playing.as_ref() == Some(&(anime.clone(), episode.clone()));
                if result.is_err()
                    && pos < 2.0
                    && still_current
                    && self.playback_attempt < self.playback_queue.len()
                {
                    self.advance_playback().await;
                } else {
                    match result {
                        Ok(()) => self.app.set_status("playback finished"),
                        Err(e) => self.app.set_error(format!("playback error: {e}")),
                    }
                    if still_current {
                        self.playing = None;
                    }
                    if self.app.view == View::Player {
                        self.app.view = View::Episodes;
                    }
                    self.app.loading = false;
                }
            }
            Msg::PlayQueue { anime, episode, queue } => {
                self.start_playback_queue(anime, episode, queue).await;
            }
            Msg::Prefetched { episode, sources } => {
                if self.prefetch_inflight.as_deref() == Some(episode.0.as_str()) {
                    self.prefetch_inflight = None;
                }
                if !sources.is_empty() {
                    self.source_cache.insert(episode.0, sources);
                }
            }
            Msg::Poster { anime, image } => {
                if self.poster_inflight.as_deref() == Some(anime.0.as_str()) {
                    self.poster_inflight = None;
                }
                // Apply only if it's still the subject being shown (details anime).
                if self.poster_subject_id().as_deref() == Some(anime.0.as_str()) {
                    if let Ok(img) = image {
                        self.current_poster = Some((anime, img));
                        self.poster_dirty = true;
                    }
                }
            }
            Msg::Thumb { anime, png } => {
                self.thumb_inflight.remove(&anime.0);
                if let Ok(png) = png {
                    // Stored → the next `place_thumbnails` pass shows it immediately.
                    self.thumb_cache.insert(anime.0, png);
                }
            }
        }
    }

    /// Set up an ordered fallback queue for an episode and start the first source.
    async fn start_playback_queue(
        &mut self,
        anime: AnimeId,
        episode: EpisodeId,
        queue: Vec<PreparedSource>,
    ) {
        self.playing = Some((anime, episode));
        self.playback_queue = queue;
        self.playback_attempt = 0;
        self.advance_playback().await;
    }

    /// Start the next candidate in `playback_queue`. For the embedded backend a
    /// synchronous start failure immediately tries the following candidate; for the
    /// external backend a failure arrives later as `Msg::ExternalEnded`, which calls
    /// back here. When the queue is exhausted, surface the failure.
    async fn advance_playback(&mut self) {
        let Some((anime, episode)) = self.playing.clone() else {
            return;
        };
        loop {
            let idx = self.playback_attempt;
            let Some(source) = self.playback_queue.get(idx).cloned() else {
                self.app.set_error("all sources failed to load");
                if self.app.view == View::Player {
                    self.app.view = View::Episodes;
                }
                self.app.loading = false;
                self.playing = None;
                return;
            };
            self.playback_attempt += 1;
            if idx > 0 {
                // A previous candidate failed — tell the user what we're trying now.
                let host = source.label.clone().unwrap_or_else(|| "next source".into());
                self.app.set_status(format!("trying source {host}…"));
            }
            if self.begin_playback(anime.clone(), episode.clone(), source).await {
                return; // running (embedded) or spawned (external)
            }
            // Embedded start failed synchronously → try the next candidate.
        }
    }

    /// Start playing one source. Returns `true` if it started (embedded) or was
    /// spawned (external), `false` if the embedded backend failed to start (so the
    /// caller can fall back to the next candidate).
    async fn begin_playback(
        &mut self,
        anime: AnimeId,
        episode: EpisodeId,
        source: PreparedSource,
    ) -> bool {
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
                        true
                    }
                    Err(e) => {
                        // Let the caller try the next candidate.
                        self.app.set_error(format!("player error: {e}"));
                        self.app.playback = None;
                        false
                    }
                }
            }
            Backend::ExternalMpv => {
                let (mpv, tx, prog_tx, conf) = (
                    self.mpv_path.clone(),
                    self.tx.clone(),
                    self.prog_tx.clone(),
                    self.input_conf_path.clone(),
                );
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
                        conf,
                        prog_tx,
                    )
                    .await;
                    let _ = tx.send(Msg::ExternalEnded { anime, episode, result });
                });
                self.app.set_status("playing (external mpv)");
                true
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
        // Age transient status messages and advance the loading spinner.
        self.app.tick_status();
        if self.app.loading || self.app.loading_more {
            self.app.advance_spinner();
        }
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

    /// Spawn a background task to fetch/decode/cache an anime's details poster
    /// (Kitty terminals only). Reports `Msg::Poster` when the decoded image is ready.
    fn fetch_poster(&mut self, anime: AnimeId, url: String) {
        if !crate::player::kitty::probe_support() {
            return; // no graphics support → text-only
        }
        self.poster_inflight = Some(anime.0.clone());
        let cache_path = self.poster_cache.path_for(&format!("{}.poster", anime.0));
        let (tx, http) = (self.tx.clone(), self.http.clone());
        tokio::spawn(async move {
            let image = fetch_poster_image(&http, &cache_path, &url).await;
            let _ = tx.send(Msg::Poster { anime, image });
        });
    }

    /// The anime whose (big) poster should currently be shown: the details anime.
    fn poster_subject_id(&self) -> Option<String> {
        match self.app.view {
            View::Details => self.app.details.as_ref().map(|d| d.id.0.clone()),
            _ => None,
        }
    }

    /// Spawn a background task to fetch a small row thumbnail (already sized down),
    /// reported as `Msg::Thumb`. Bounded concurrency via `thumb_inflight`.
    fn fetch_thumb(&mut self, anime: AnimeId, url: String) {
        self.thumb_inflight.insert(anime.0.clone());
        let cache_path = self.poster_cache.path_for(&format!("{}.thumb", anime.0));
        let (tx, http) = (self.tx.clone(), self.http.clone());
        tokio::spawn(async move {
            let png = fetch_thumb_png(&http, &cache_path, &url).await;
            let _ = tx.send(Msg::Thumb { anime, png });
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
        self.app.loading_more = false;
        self.app.set_error(format!("error: {e}"));
    }
}

/// Current reserved video rectangle, from the live terminal size + fullscreen flag.
fn current_video_rect(fullscreen: bool) -> crate::player::kitty::CellRect {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    ui::video_rect(Rect::new(0, 0, cols, rows), fullscreen)
}

/// Minimally adjust `offset` so `selected` stays within `[offset, offset+visible)`,
/// then clamp so the last page isn't scrolled past the end. Pure → unit-testable.
fn keep_in_view(mut offset: usize, selected: usize, visible: usize, len: usize) -> usize {
    let visible = visible.max(1);
    if selected < offset {
        offset = selected;
    } else if selected >= offset + visible {
        offset = selected + 1 - visible;
    }
    offset.min(len.saturating_sub(visible))
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
/// Resolve an embed-page URL to a direct stream via yt-dlp, capturing BOTH the URL
/// and the per-stream HTTP headers yt-dlp says it needs (Referer/User-Agent/…).
///
/// This is the fix for hosts like sibnet whose CDN 403s unless the request carries
/// the host's own Referer: `yt-dlp -g` used to return only the URL, so we replayed
/// it with the generic site referer and the CDN rejected it. `referer` (the embed
/// page's referer) is passed to yt-dlp so extractors that gate on it still work.
///
/// Returns `(url, Some(headers))` on success; `(original_url, None)` when yt-dlp
/// fails or it's already a direct/local stream (mpv's own ytdl hook then handles a
/// page URL as a last resort).
async fn pre_resolve(url: &str, referer: Option<&str>) -> (String, Option<Vec<(String, String)>>) {
    if url.starts_with("file://") || is_direct_stream(url) {
        // Already a playable stream (local file or direct HLS/MP4) — skip yt-dlp
        // entirely; spawning it here would just add ~1-2 s of latency.
        return (url.to_string(), None);
    }
    let mut args: Vec<String> = vec![
        "--no-playlist".into(),
        "--quiet".into(),
        "--no-warnings".into(),
        "-f".into(),
        "best".into(),
        // First line: the direct stream URL. Second line: the format's HTTP headers
        // as JSON, so we replay the stream with exactly what yt-dlp would send.
        "--print".into(),
        "%(url)s".into(),
        "--print".into(),
        "%(http_headers)j".into(),
    ];
    if let Some(r) = referer {
        args.push("--referer".into());
        args.push(r.to_string());
    }
    args.push(url.to_string());

    let Ok(out) = tokio::process::Command::new("yt-dlp")
        .args(&args)
        .output()
        .await
    else {
        return (url.to_string(), None);
    };
    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut lines = stdout.lines();
        if let Some(direct) = lines.next().map(str::trim) {
            if direct.starts_with("http://") || direct.starts_with("https://") {
                let headers = lines.next().and_then(parse_ytdl_headers);
                return (direct.to_string(), headers);
            }
        }
    }
    (url.to_string(), None)
}

/// Parse yt-dlp's `%(http_headers)j` output (a JSON object of header→value) into an
/// ordered header list. Returns `None` if it isn't a non-empty JSON object.
fn parse_ytdl_headers(json: &str) -> Option<Vec<(String, String)>> {
    let map: serde_json::Map<String, Value> = serde_json::from_str(json.trim()).ok()?;
    let headers: Vec<(String, String)> = map
        .into_iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
        .collect();
    (!headers.is_empty()).then_some(headers)
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
    let mut prepared: Vec<PreparedSource> = sources
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
    // Try known-resolvable hosts first (and, on failure, fall back in this order).
    // Stable sort preserves the provider's per-host language ordering.
    prepared.sort_by_key(|s| host_rank(s.label.as_deref().unwrap_or("")));
    Ok(prepared)
}

/// Index of the source best matching the `preferred` label (e.g. "vidmoly (VF)"),
/// or `0` when none matches. Matching is by tokens (host + language) contained in
/// the source label, case-insensitively. `sources` is already reliability-ordered,
/// so index 0 is the sensible fallback.
fn pick_default_index(sources: &[PreparedSource], preferred: &str) -> usize {
    let tokens: Vec<String> = preferred
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect();
    if !tokens.is_empty() {
        if let Some(i) = sources.iter().position(|s| {
            let label = s.label.as_deref().unwrap_or("").to_lowercase();
            tokens.iter().all(|t| label.contains(t.as_str()))
        }) {
            return i;
        }
    }
    0
}

/// Reliability rank for a source host (lower = try first). yt-dlp resolves
/// vidmoly (generic) and sibnet (dedicated extractor); voe has no working
/// extractor today, so it sorts last and is only reached via fallback.
fn host_rank(label: &str) -> u8 {
    let l = label.to_ascii_lowercase();
    if l.contains("voe") {
        3
    } else if l.contains("vidmoly") {
        0
    } else if l.contains("sibnet") {
        1
    } else {
        2
    }
}

/// Raw image bytes from the on-disk cache, else fetched from `url` (via the shared
/// client, for connection reuse) and cached.
async fn load_or_fetch_bytes(
    client: &reqwest::Client,
    cache_path: &std::path::Path,
    url: &str,
) -> Result<Vec<u8>> {
    if let Ok(bytes) = tokio::fs::read(cache_path).await {
        if !bytes.is_empty() {
            return Ok(bytes);
        }
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(Error::InvalidUrl(url.to_string()));
    }
    let raw = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Network(format!("image fetch: {e}")))?
        .bytes()
        .await
        .map_err(|e| Error::Network(format!("image body: {e}")))?;
    let _ = tokio::fs::write(cache_path, &raw).await;
    Ok(raw.to_vec())
}

/// Fetch (or load) and decode a poster into a `DynamicImage`, capped to a sane max
/// so a huge source doesn't blow up memory. Decoding runs on a blocking thread.
async fn fetch_poster_image(
    client: &reqwest::Client,
    cache_path: &std::path::Path,
    url: &str,
) -> Result<image::DynamicImage> {
    let bytes = load_or_fetch_bytes(client, cache_path, url).await?;
    tokio::task::spawn_blocking(move || -> Result<image::DynamicImage> {
        let img = image::load_from_memory(&bytes)
            .map_err(|e| Error::Resolve(format!("poster decode: {e}")))?;
        // Plenty of detail for any on-screen box while bounding memory/CPU.
        let capped = if img.width() > 900 || img.height() > 900 {
            img.resize(900, 900, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        Ok(capped)
    })
    .await
    .map_err(|e| Error::Resolve(format!("poster task: {e}")))?
}

/// Fetch (or load), decode, and downscale a browse thumbnail to a small PNG. Small
/// + low-res is fine for row thumbnails and keeps it fast.
async fn fetch_thumb_png(
    client: &reqwest::Client,
    cache_path: &std::path::Path,
    url: &str,
) -> Result<Vec<u8>> {
    let bytes = load_or_fetch_bytes(client, cache_path, url).await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let img = image::load_from_memory(&bytes)
            .map_err(|e| Error::Resolve(format!("thumb decode: {e}")))?;
        let small = img.resize_to_fill(120, 180, image::imageops::FilterType::Triangle);
        let mut out = std::io::Cursor::new(Vec::new());
        small
            .write_to(&mut out, image::ImageFormat::Png)
            .map_err(|e| Error::Resolve(format!("thumb encode: {e}")))?;
        Ok(out.into_inner())
    })
    .await
    .map_err(|e| Error::Resolve(format!("thumb task: {e}")))?
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
                let referer = referer_of(&s.headers);
                async move { pre_resolve(&url, referer.as_deref()).await }
            })
            .collect::<Vec<_>>(),
    )
    .await;
    for (s, (u, headers)) in sources.iter_mut().zip(resolved) {
        s.url = u;
        // Prefer yt-dlp's per-stream headers (correct Referer/UA for the CDN); keep
        // the site referer only when resolution didn't yield any (page fallback).
        if let Some(h) = headers {
            s.headers = h;
        }
    }
    Ok(sources)
}

/// The `Referer` value from a header list, if present (case-insensitive key).
fn referer_of(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("referer"))
        .map(|(_, v)| v.clone())
}

#[cfg(test)]
mod tests {
    use super::{host_rank, keep_in_view, parse_ytdl_headers, pick_default_index, referer_of, PreparedSource};

    fn src(label: &str) -> PreparedSource {
        PreparedSource { url: "https://x/v".into(), headers: vec![], label: Some(label.into()) }
    }

    #[test]
    fn pick_default_matches_host_and_language() {
        let sources = vec![src("vidmoly (VOSTFR)"), src("sibnet (VF)"), src("vidmoly (VF)")];
        // Host + language both must match.
        assert_eq!(pick_default_index(&sources, "vidmoly (VF)"), 2);
        assert_eq!(pick_default_index(&sources, "sibnet (VF)"), 1);
        // No match → 0 (list is already reliability-ordered).
        assert_eq!(pick_default_index(&sources, "voe (VF)"), 0);
        assert_eq!(pick_default_index(&sources, ""), 0);
    }

    #[test]
    fn host_rank_orders_working_hosts_first_voe_last() {
        // Reliability order: vidmoly < sibnet < unknown < voe.
        assert!(host_rank("vidmoly (VF)") < host_rank("sibnet (VF)"));
        assert!(host_rank("sibnet (VF)") < host_rank("mystery (VOSTFR)"));
        assert!(host_rank("mystery (VOSTFR)") < host_rank("voe (VOSTFR)"));

        // A mixed list sorts voe to the end, stably preserving other order.
        let mut labels = vec!["voe (VF)", "vidmoly (VF)", "sibnet (VF)"];
        labels.sort_by_key(|l| host_rank(l));
        assert_eq!(labels, vec!["vidmoly (VF)", "sibnet (VF)", "voe (VF)"]);
    }

    #[test]
    fn parse_ytdl_headers_reads_json_map() {
        let h = parse_ytdl_headers(r#"{"Referer":"https://sibnet.ru/","User-Agent":"x"}"#).unwrap();
        assert!(h.iter().any(|(k, v)| k == "Referer" && v == "https://sibnet.ru/"));
        assert!(h.iter().any(|(k, v)| k == "User-Agent" && v == "x"));
        // Empty / non-object → None (fall back to keeping existing headers).
        assert!(parse_ytdl_headers("{}").is_none());
        assert!(parse_ytdl_headers("not json").is_none());
    }

    #[test]
    fn referer_of_is_case_insensitive() {
        let h = vec![("referer".to_string(), "https://nakanime.tv/".to_string())];
        assert_eq!(referer_of(&h).as_deref(), Some("https://nakanime.tv/"));
        assert!(referer_of(&[]).is_none());
    }

    #[test]
    fn keep_in_view_scrolls_only_when_needed() {
        // Selection within the window → offset unchanged.
        assert_eq!(keep_in_view(0, 3, 10, 100), 0);
        // Selection below the window → scroll down just enough.
        assert_eq!(keep_in_view(0, 12, 10, 100), 3); // 12 - 10 + 1
        // Selection above the window → scroll up to it.
        assert_eq!(keep_in_view(20, 5, 10, 100), 5);
        // Clamped so the last page doesn't scroll past the end.
        assert_eq!(keep_in_view(95, 99, 10, 100), 90);
        // Fewer items than the window → offset 0.
        assert_eq!(keep_in_view(0, 2, 10, 3), 0);
    }
}
