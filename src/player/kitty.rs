//! Kitty-graphics helpers: capability detection, cell-based placement geometry,
//! and cleanup escape sequences.
//!
//! For VIDEO we do NOT emit image data ourselves — mpv's `--vo=kitty` does that.
//! Our job there is (a) decide whether embedded playback is possible, (b) compute
//! the cell rectangle to hand mpv, and (c) guarantee image placements are deleted
//! on every exit path so the terminal is left clean.
//!
//! For the STATIC anime poster we do transmit the image ourselves via
//! [`transmit_png`] — a single decoded cover, unrelated to the video pipeline.

use base64::Engine as _;

/// A rectangle in terminal *cells* (columns/rows), matching ratatui's `Rect`
/// and mpv's `--vo-kitty-left/top/cols/rows` coordinate space.
///
/// `pixel_width`/`pixel_height` carry the actual pixel dimensions of the area
/// when available (queried via `TIOCGWINSZ`). mpv uses these for exact pixel
/// sizing, avoiding cell-size rounding artefacts and unnecessary rescaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRect {
    pub left: u16,
    pub top: u16,
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: Option<u16>,
    pub pixel_height: Option<u16>,
}

impl CellRect {
    pub fn is_empty(&self) -> bool {
        self.cols == 0 || self.rows == 0
    }
}

/// Escape sequence that deletes ALL Kitty image placements (`_Ga=d,d=A`).
/// Must be written to the terminal on cleanup / panic / resize-restart.
pub const DELETE_ALL_IMAGES: &str = "\x1b_Ga=d,d=A\x1b\\";

/// Fixed image id used for the anime poster, so it can be replaced/deleted on its
/// own without disturbing any mpv image placements.
pub const POSTER_ID: u32 = 7;

/// Delete just the poster image (by id), leaving other placements alone.
pub const DELETE_POSTER: &str = "\x1b_Ga=d,d=i,i=7\x1b\\";

/// Build the escape sequence that places a PNG, scaled into `rect.cols × rect.rows`
/// cells at `rect.left/top`, using the fixed [`POSTER_ID`]. The cursor is saved,
/// moved to the rect's top-left, the image transmitted+displayed in chunks, then
/// the cursor is restored — so it never disturbs the TUI's cursor state.
///
/// Kitty requires the base64 payload split into ≤4096-byte chunks with `m=1` on
/// every chunk but the last. Pure string builder → no IO, unit-testable.
pub fn transmit_png(png: &[u8], rect: CellRect) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let chunks: Vec<&str> = chunk_str(&b64, 4096);

    // Save cursor, then move to (row=top+1, col=left+1) — CUP is 1-based.
    let mut out = String::with_capacity(b64.len() + 128);
    out.push_str("\x1b7"); // DECSC save cursor
    out.push_str(&format!("\x1b[{};{}H", rect.top + 1, rect.left + 1));

    if chunks.is_empty() {
        out.push_str("\x1b8");
        return out;
    }

    for (i, chunk) in chunks.iter().enumerate() {
        let last = i + 1 == chunks.len();
        let m = if last { 0 } else { 1 };
        if i == 0 {
            // First chunk carries the control keys: transmit+display (a=T), PNG
            // (f=100), our id, and scale-to-cell-box (c=cols, r=rows).
            out.push_str(&format!(
                "\x1b_Ga=T,f=100,i={},c={},r={},m={};{}\x1b\\",
                POSTER_ID, rect.cols, rect.rows, m, chunk
            ));
        } else {
            out.push_str(&format!("\x1b_Gm={};{}\x1b\\", m, chunk));
        }
    }
    out.push_str("\x1b8"); // DECRC restore cursor
    out
}

fn chunk_str(s: &str, n: usize) -> Vec<&str> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let end = (i + n).min(s.len());
        v.push(&s[i..end]);
        i = end;
    }
    v
}

/// Best-effort detection of Kitty graphics support.
///
/// Ghostty, Kitty and WezTerm implement the protocol. `KITTY_WINDOW_ID` is set
/// by Kitty; `TERM`/`TERM_PROGRAM` cover Ghostty and WezTerm. A robust runtime
/// probe (send a 1px image + query, await response) is a TODO for the real
/// player; this heuristic decides the default backend without blocking startup.
pub fn probe_support() -> bool {
    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return true;
    }
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let prog = std::env::var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    term.contains("kitty")
        || term.contains("ghostty")
        || term.contains("wezterm")
        || prog.contains("ghostty")
        || prog.contains("wezterm")
        || prog.contains("kitty")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_all_is_well_formed() {
        assert!(DELETE_ALL_IMAGES.starts_with("\x1b_G"));
        assert!(DELETE_ALL_IMAGES.ends_with("\x1b\\"));
    }

    #[test]
    fn empty_rect_detected() {
        assert!(CellRect { left: 0, top: 0, cols: 0, rows: 10, pixel_width: None, pixel_height: None }.is_empty());
        assert!(!CellRect { left: 1, top: 1, cols: 40, rows: 20, pixel_width: None, pixel_height: None }.is_empty());
    }

    #[test]
    fn delete_poster_is_well_formed() {
        assert!(DELETE_POSTER.starts_with("\x1b_G"));
        assert!(DELETE_POSTER.ends_with("\x1b\\"));
        assert!(DELETE_POSTER.contains(&format!("i={POSTER_ID}")));
    }

    #[test]
    fn transmit_png_places_and_chunks() {
        // ~9336 base64 chars → 3 chunks (control + a middle m=1 + final m=0).
        let png = vec![0u8; 7000];
        let rect = CellRect { left: 3, top: 2, cols: 20, rows: 30, pixel_width: None, pixel_height: None };
        let s = transmit_png(&png, rect);
        // Saves cursor and moves to 1-based (row=3, col=4).
        assert!(s.starts_with("\x1b7\x1b[3;4H"));
        // First chunk carries the control keys.
        assert!(s.contains(&format!("a=T,f=100,i={POSTER_ID},c=20,r=30,m=1;")));
        // Continuation chunk with just m=.
        assert!(s.contains("\x1b_Gm=1;"));
        // Ends by restoring the cursor after a final chunk (m=0).
        assert!(s.contains("m=0;"));
        assert!(s.ends_with("\x1b8"));
    }
}
