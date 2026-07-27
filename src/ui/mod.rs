//! Rendering. Pure view logic: takes `&App` and a ratatui `Frame`, draws widgets.
//! The one rule that keeps embedded playback working: in [`View::Player`] we
//! reserve a rectangle and (for embedded output) draw nothing inside it, so
//! mpv's Kitty output is not overpainted by TUI redraws.

use crate::app::{self, App, View};
use crate::player::kitty::CellRect;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // In fullscreen player mode the video occupies the entire terminal.
    // Fill the whole area with black so letterbox bars (areas mpv doesn't
    // paint because of aspect-ratio scaling) appear black rather than showing
    // the terminal background colour. Kitty images render above the text
    // layer, so this black fill doesn't interfere with the video itself.
    if app.view == View::Player && app.fullscreen {
        frame.render_widget(
            ratatui::widgets::Block::default()
                .style(ratatui::style::Style::default().bg(ratatui::style::Color::Black)),
            area,
        );
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(1),    // body
        Constraint::Length(1), // status/footer
    ])
    .split(area);

    render_header(frame, app, rows[0]);
    match app.view {
        View::Details => render_details(frame, app, rows[1]),
        View::Episodes => render_episodes(frame, app, rows[1]),
        View::Sources => render_sources(frame, app, rows[1]),
        View::Player => render_player(frame, app, rows[1]),
        _ => render_list(frame, app, rows[1]),
    }
    render_footer(frame, app, rows[2]);
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let tab_label = match app.view {
        View::Home | View::Search => "[ Home ]  Favourites   History ",
        View::Favourites => "  Home  [ Favourites ]  History ",
        View::History => "  Home    Favourites  [ History ]",
        _ => "",
    };
    let title = if tab_label.is_empty() {
        format!(" anime-tui — {:?} ", app.view)
    } else {
        format!(" anime-tui   {tab_label}")
    };
    frame.render_widget(
        Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
        area,
    );
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let text = if app.loading {
        " loading... ".to_string()
    } else if app.input_mode && app.filtering {
        format!(" filter: {}|  (Enter keep · Esc clear)", app.filter)
    } else if app.input_mode {
        format!(" search: {}|", app.search_input)
    } else {
        let hint = match app.view {
            View::Home | View::Search => {
                " / search · j/k move · Enter select · S sort · F filter · Tab tab · q quit"
            }
            View::Favourites | View::History => {
                " j/k move · Enter select · F filter · Tab tab · Esc back · q quit"
            }
            View::Details => " Enter episodes · f toggle favourite · Esc back · q quit",
            View::Episodes => " Enter play / fold season · j/k move · Esc back · q quit",
            View::Sources => " Enter select source · j/k move · Esc back",
            View::Player => " Space pause · ,/. ±5s · h/l ±10s · i skip intro · g seek · o window · f fullscreen · n/p ep · q stop",
        };
        match app.status.as_deref() {
            Some(s) => format!(" {s}  ·  {}", hint.trim()),
            None => hint.to_string(),
        }
    };
    frame.render_widget(Paragraph::new(text).style(Style::default().dim()), area);
}

/// Browse-list block title: base label plus filter/count/sort/loading context.
fn browse_title(app: &App) -> String {
    let base = match app.view {
        View::Favourites => "Favourites",
        View::History => "History",
        _ => "Results",
    };
    let shown = app.results.len();
    if app.filtering || !app.filter.is_empty() {
        return format!(" {base} — filter '{}': {shown} shown ", app.filter);
    }
    // Catalogue views show pagination/sort context; local lists just a count.
    if matches!(app.view, View::Home | View::Search) {
        let more = if app.loading_more { " · loading more…" } else { "" };
        format!(" {base} — {shown}/{} · {}{more} ", app.total, app.sort.label())
    } else {
        format!(" {base} ({shown}) ")
    }
}

fn render_list(frame: &mut Frame, app: &App, area: Rect) {
    let title = browse_title(app);

    if app.results.is_empty() {
        let msg = match app.view {
            View::Favourites => "No favourites yet. Press `f` on a details page to add one.",
            View::History => "No history yet. Watch an episode to record it here.",
            _ if app.input_mode => "Type a query and press Enter.",
            _ => "Press `/` to search, or Tab to browse Favourites / History.",
        };
        frame.render_widget(
            Paragraph::new(msg)
                .alignment(Alignment::Center)
                .block(Block::default().title(title).borders(Borders::ALL)),
            area,
        );
        return;
    }

    // On a Kitty terminal, reserve a small left column on each row for the cover
    // thumbnail (painted by the run loop) and make rows tall enough to show it.
    let thumbs = crate::player::kitty::probe_support();
    let pad = if thumbs { " ".repeat((THUMB_COLS + 1) as usize) } else { String::new() };

    let items: Vec<ListItem> = app
        .results
        .iter()
        .map(|a| {
            let year = a.year.map(|y| format!(" ({y})")).unwrap_or_default();
            if thumbs {
                // ROW_H lines tall: title, dim year, then blanks — text offset past
                // the thumbnail column so it never overlaps the image.
                let mut lines = vec![Line::from(format!("{pad}{}", a.title))];
                if !year.is_empty() {
                    lines.push(Line::from(format!("{pad}{}", year.trim())).dim());
                }
                while lines.len() < ROW_H as usize {
                    lines.push(Line::from(""));
                }
                ListItem::new(lines)
            } else {
                ListItem::new(format!("{}{}", a.title, year))
            }
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    // Control the scroll so it matches the runner's `list_offset` (used to place
    // the row thumbnails). No highlight symbol — the thumbnail column owns the left.
    let mut state = app
        .results_state
        .clone()
        .with_offset(if thumbs { app.list_offset } else { 0 });
    frame.render_stateful_widget(list, area, &mut state);
}

/// Thumbnail column width (cells) and per-row height (rows) in the browse list.
/// Bigger rows → bigger covers, fewer titles per screen.
pub const THUMB_COLS: u16 = 8;
pub const ROW_H: u16 = 5;

fn render_details(frame: &mut Frame, app: &App, area: Rect) {
    let Some(d) = &app.details else {
        frame.render_widget(centered("loading details...", true), area);
        return;
    };
    // Reserve the poster column on the LEFT (painted by the run loop, never
    // overpainted here); metadata goes to its right. Full width when no poster.
    let meta_area = match side_poster(area, false) {
        Some(p) => Rect {
            x: area.x + p.width + 2,
            y: area.y,
            width: area.width.saturating_sub(p.width + 2),
            height: area.height,
        },
        None => area,
    };
    let fav_marker = if app.is_favourite { " [*]" } else { " [ ]" };
    let mut lines = vec![
        Line::from(format!("{}{}", d.title, fav_marker))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        Line::from(""),
    ];
    if !d.genres.is_empty() {
        lines.push(Line::from(format!("Genres: {}", d.genres.join(", "))));
    }
    if let Some(status) = &d.status {
        lines.push(Line::from(format!("Status: {status}")));
    }
    lines.push(Line::from(format!("Episodes: {}", d.episodes.len())));
    lines.push(Line::from(""));
    if let Some(desc) = &d.description {
        lines.push(Line::from(desc.clone()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from("Enter = view episodes  f = toggle favourite  Esc = back").dim());

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().title(" Details ").borders(Borders::ALL)),
        meta_area,
    );
}

/// Minimum columns kept for the metadata so the details text stays readable, and
/// the smallest poster worth drawing.
const MIN_INFO_COLS: u16 = 32;
const MIN_POSTER_COLS: u16 = 12;

/// Pixels-per-cell `(width, height)` from the terminal, or `(0, 0)` if unknown.
fn cell_pixel_size() -> (u16, u16) {
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.width > 0 && ws.columns > 0 && ws.height > 0 && ws.rows > 0 {
            return (ws.width / ws.columns, ws.height / ws.rows);
        }
    }
    (0, 0)
}

/// The poster rect (in cells) on one side of `body`, or `None` when there's no
/// Kitty support or the terminal is too small. `right` places it against the
/// right edge; otherwise the left edge (details page uses the left).
///
/// Responsive: the poster is the LARGEST aspect-correct (~2:3) cover that fits —
/// capped to ~55% width and to leaving [`MIN_INFO_COLS`] for the text — so it grows
/// on big terminals and disappears on small ones. Shared by `render_details` (which
/// reserves the column) and `details_poster_rect` (which the run loop transmits into)
/// so the two never disagree.
fn side_poster(body: Rect, right: bool) -> Option<Rect> {
    if !crate::player::kitty::probe_support() {
        return None;
    }
    // Fallback cell aspect ~1:2 (w:h) when the terminal doesn't report pixels.
    let (cw, ch) = match cell_pixel_size() {
        (0, _) | (_, 0) => (1u32, 2u32),
        (w, h) => (w as u32, h as u32),
    };
    let (cols, rows) = poster_size(body, cw, ch)?;
    Some(place_poster(body, cols, rows, right))
}

/// Position a `cols × rows` poster against the left or right edge of `body`.
fn place_poster(body: Rect, cols: u16, rows: u16, right: bool) -> Rect {
    let x = if right {
        body.x + body.width.saturating_sub(cols)
    } else {
        body.x
    };
    Rect { x, y: body.y, width: cols, height: rows }
}

/// Pure sizing: the (cols, rows) of the largest 2:3 poster fitting `body`, given
/// pixels-per-cell `(cw, ch)`. Env-free and side-agnostic → unit-testable.
fn poster_size(body: Rect, cw: u32, ch: u32) -> Option<(u16, u16)> {
    if body.width < MIN_POSTER_COLS + MIN_INFO_COLS + 2 || body.height < 10 {
        return None; // keep the text readable on small terminals
    }
    let (bw, bh) = (body.width as u32, body.height as u32);

    // Width (cells) of a 2:3 poster that is exactly `body.height` tall.
    let height_limited = ((bh * ch) * 2 / 3) / cw.max(1);
    // Width cap: at most ~55% of the body, and always leave MIN_INFO_COLS for text.
    let width_cap = (bw * 55 / 100).min(bw.saturating_sub(MIN_INFO_COLS as u32 + 2));
    let cols = height_limited.min(width_cap).max(MIN_POSTER_COLS as u32);
    // Rows back-derived from the chosen width so it stays 2:3 and never overflows.
    let rows = (((cols * cw) * 3 / 2) / ch.max(1)).min(bh);

    Some((cols as u16, rows.max(3) as u16))
}

/// The body area (between header and footer) of the full terminal `area`.
fn body_of(area: Rect) -> Rect {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area)[1]
}

fn to_cellrect(r: Rect) -> CellRect {
    let (cw, ch) = cell_pixel_size();
    let (pw, ph) = if cw > 0 && ch > 0 {
        (Some(r.width.saturating_mul(cw)), Some(r.height.saturating_mul(ch)))
    } else {
        (None, None)
    };
    CellRect { left: r.x, top: r.y, cols: r.width, rows: r.height, pixel_width: pw, pixel_height: ph }
}

const EMPTY_CELLRECT: CellRect =
    CellRect { left: 0, top: 0, cols: 0, rows: 0, pixel_width: None, pixel_height: None };

/// Details-page poster rect (LEFT). MUST match `render_details`'s reserved column.
pub fn details_poster_rect(area: Rect) -> CellRect {
    side_poster(body_of(area), false).map(to_cellrect).unwrap_or(EMPTY_CELLRECT)
}

/// The body area (between header and footer) of the full terminal `area` — used by
/// the runner to place per-row browse thumbnails against `render_list`'s layout.
pub fn browse_body(area: Rect) -> Rect {
    body_of(area)
}

fn render_episodes(frame: &mut Frame, app: &App, area: Rect) {
    use crate::app::EpisodeRow;

    let episodes: &[crate::models::Episode] = app
        .details
        .as_ref()
        .map(|d| d.episodes.as_slice())
        .unwrap_or_default();

    let items: Vec<ListItem> = app
        .episode_rows()
        .into_iter()
        .map(|row| match row {
            // Foldable season header. ▼ = open, ▶ = folded.
            EpisodeRow::Season { id, rank, expanded, episode_count } => {
                let arrow = if expanded { "▼" } else { "▶" };
                // How many episodes in this season are in progress — so a folded
                // season still shows that it has resume positions inside.
                let in_progress = episodes
                    .iter()
                    .filter(|e| {
                        e.season_id.unwrap_or(0) == id
                            && app.resume_positions.get(&e.id.0).copied().unwrap_or(0.0) > 1.0
                    })
                    .count();
                let progress = if in_progress > 0 {
                    format!("  ▶{in_progress}")
                } else {
                    String::new()
                };
                ListItem::new(format!(
                    "{arrow} Season {rank}  ({episode_count} ep){progress}"
                ))
                .style(Style::default().add_modifier(Modifier::BOLD))
            }
            EpisodeRow::Episode { index, indented } => {
                let e = &episodes[index];
                let title = e.title.as_deref().unwrap_or("");
                let resume = app.resume_positions.get(&e.id.0).copied().unwrap_or(0.0);
                let resume_str = if resume > 1.0 {
                    format!("  [{}]", app::fmt_time(resume))
                } else {
                    String::new()
                };
                // Indent episodes under a season header so the tree reads clearly.
                let indent = if indented { "  " } else { "" };
                let label = if title.is_empty() {
                    format!("{indent}Ep {}{}", e.number, resume_str)
                } else {
                    format!("{indent}Ep {} - {}{}", e.number, title, resume_str)
                };
                ListItem::new(label)
            }
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().title(" Episodes ").borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    let mut state = app.episodes_state.clone();
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_sources(frame: &mut Frame, app: &App, area: Rect) {
    if app.source_labels.is_empty() {
        frame.render_widget(centered("No sources available.", false), area);
        return;
    }

    let items: Vec<ListItem> = app
        .source_labels
        .iter()
        .map(|label| ListItem::new(label.clone()))
        .collect();

    let list = List::new(items)
        .block(Block::default().title(" Select Source ").borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    let mut state = app.source_state.clone();
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_player(frame: &mut Frame, app: &App, area: Rect) {
    // Split the body into the video surface and a one-row progress bar. The
    // video surface is left BLANK: with embedded Kitty output, mpv paints here
    // and TUI redraws must never overpaint it (ratatui diffing leaves untouched
    // empty cells alone). The external backend simply shows nothing here.
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

    if app.loading {
        frame.render_widget(centered("Launching mpv...", true), rows[0]);
    }

    let pb = app.playback.unwrap_or_default();
    let fps_str = pb.fps.map(|f| format!(" · {:.0} fps", f)).unwrap_or_default();
    let label = format!(
        "{} {} / {}{}",
        if pb.paused { "||" } else { ">" },
        app::fmt_time(pb.position),
        app::fmt_time(pb.duration),
        fps_str,
    );
    let gauge = ratatui::widgets::Gauge::default()
        .gauge_style(Style::default().fg(ratatui::style::Color::Cyan))
        .ratio(pb.ratio())
        .label(label);
    frame.render_widget(gauge, rows[1]);

    // Seek-to-time input overlay
    if let Some(input) = &app.seek_input {
        let prompt = format!(" Seek to (MM:SS): {}_", input);
        let popup = Paragraph::new(prompt)
            .style(Style::default().bg(ratatui::style::Color::DarkGray));
        frame.render_widget(popup, rows[1]);
    }
}

fn centered(text: &str, dim: bool) -> Paragraph<'_> {
    let p = Paragraph::new(text)
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    if dim {
        p.dim()
    } else {
        p
    }
}

/// The cell rectangle handed to mpv for embedded playback. It MUST match the
/// video surface drawn by [`render_player`] exactly.
///
/// When `fullscreen` is true the video occupies the entire terminal; ratatui
/// draws no chrome at all and skips the TUI draw on that frame.
///
/// The pixel dimensions MUST match the real terminal size for the cell box:
/// mpv's kitty VO derives its render size from these, and any value smaller than
/// the true size makes mpv paint a shrunken image into the top-left corner rather
/// than filling the box (it does NOT upscale). To reduce memory, use the external
/// player instead of a smaller render — see `player::run_external`.
pub fn video_rect(area: Rect, fullscreen: bool) -> CellRect {
    let v = if fullscreen {
        area
    } else {
        let outer = Layout::vertical([
            Constraint::Length(1), // header
            Constraint::Min(1),    // body
            Constraint::Length(1), // footer
        ])
        .split(area);
        let inner = Layout::vertical([
            Constraint::Min(1),    // video surface
            Constraint::Length(1), // progress bar
        ])
        .split(outer[1]);
        inner[0]
    };

    // Derive pixel dimensions from TIOCGWINSZ so mpv knows the exact render
    // size without guessing font metrics. Falls back to None gracefully.
    let (pixel_width, pixel_height) =
        if let Ok(ws) = crossterm::terminal::window_size() {
            if ws.width > 0 && ws.columns > 0 && ws.height > 0 && ws.rows > 0 {
                let cell_w = ws.width / ws.columns;
                let cell_h = ws.height / ws.rows;
                (
                    Some(v.width.saturating_mul(cell_w)),
                    Some(v.height.saturating_mul(cell_h)),
                )
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

    CellRect {
        left: v.x,
        top: v.y,
        cols: v.width,
        rows: v.height,
        pixel_width,
        pixel_height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_rect_matches_player_layout() {
        // 80x24: header row 0, footer row 23, progress row 22, video rows 1..=21.
        let area = Rect::new(0, 0, 80, 24);
        let r = super::video_rect(area, false);
        assert_eq!(r.top, 1);
        assert_eq!(r.rows, 21);
        assert_eq!(r.cols, 80);
        // The video surface must not overlap the progress row (22) or footer (23).
        assert_eq!(r.top + r.rows, 22);
    }

    #[test]
    fn poster_size_is_responsive() {
        // Too small → no poster.
        assert!(poster_size(Rect::new(0, 0, 40, 20), 1, 2).is_none());
        assert!(poster_size(Rect::new(0, 0, 120, 8), 1, 2).is_none());

        // Wide body → sizeable poster, capped to ~55% and leaving room for info.
        let (bw, bh) = poster_size(Rect::new(0, 0, 200, 50), 1, 2).unwrap();
        assert!(bw >= MIN_POSTER_COLS);
        assert!(bw <= 200 * 55 / 100);
        assert!(200 - bw - 2 >= MIN_INFO_COLS); // text column stays readable
        assert!(bh <= 50); // never overflows the body vertically

        // Growing the terminal grows the poster (responsive).
        let (sw, _) = poster_size(Rect::new(0, 0, 80, 24), 1, 2).unwrap();
        assert!(bw >= sw);
    }

    #[test]
    fn place_poster_aligns_to_edge() {
        let body = Rect::new(0, 0, 200, 50);
        let (cols, rows) = poster_size(body, 1, 2).unwrap();
        let left = place_poster(body, cols, rows, false);
        let right = place_poster(body, cols, rows, true);
        assert_eq!(left.x, body.x);
        assert_eq!(right.x, body.x + body.width - cols);
        assert!(right.x > left.x); // right pane sits past the left
    }
}
