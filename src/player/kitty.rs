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

/// Starting image id for browse cover thumbnails (each anime gets its own id,
/// counting up from here). Distinct from [`POSTER_ID`] so they never clash.
pub const THUMB_ID_BASE: u32 = 100;

/// Delete a single Kitty image by id (and its placements), keeping the data.
pub fn delete_image(id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={id}\x1b\\")
}

/// Transmit (store) a PNG image under `id` WITHOUT displaying it (`a=t`). Chunked
/// (`m=1` on all but the last), quiet (`q=2`). Cursor is not touched. Display it
/// later with [`place`] — so scrolling only moves cheap placements, never re-sends
/// image data.
pub fn transmit_data(png: &[u8], id: u32) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let chunks = chunk_str(&b64, 4096);
    let mut out = String::with_capacity(b64.len() + 64);
    if chunks.is_empty() {
        return out;
    }
    for (i, chunk) in chunks.iter().enumerate() {
        let m = u8::from(i + 1 != chunks.len());
        if i == 0 {
            out.push_str(&format!("\x1b_Ga=t,i={id},f=100,q=2,m={m};{chunk}\x1b\\"));
        } else {
            out.push_str(&format!("\x1b_Gm={m},q=2;{chunk}\x1b\\"));
        }
    }
    out
}

/// Create a placement of the already-transmitted image `id` (see [`transmit_data`])
/// scaled into `rect.cols × rect.rows` cells at `rect.left/top`, without moving the
/// cursor (`C=1`). `placement_id` lets us delete just this placement later.
pub fn place(id: u32, placement_id: u32, rect: CellRect) -> String {
    format!(
        "\x1b7\x1b[{};{}H\x1b_Ga=p,i={},p={},c={},r={},C=1,q=2\x1b\\\x1b8",
        rect.top + 1,
        rect.left + 1,
        id,
        placement_id,
        rect.cols,
        rect.rows,
    )
}

/// Delete one placement of image `id` (lowercase `d=i` → keep the image data so it
/// can be re-placed on scroll without re-transmitting).
pub fn delete_placement(id: u32, placement_id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={id},p={placement_id},q=2\x1b\\")
}

/// Free image `id`'s stored data entirely (uppercase `d=I`) — used to bound terminal
/// memory when the browse result set changes.
pub fn free_image(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

/// Place the details poster PNG into `rect` using the fixed [`POSTER_ID`].
pub fn transmit_png(png: &[u8], rect: CellRect) -> String {
    transmit_png_id(png, rect, POSTER_ID)
}

/// Build the escape sequence that places a PNG, scaled into `rect.cols × rect.rows`
/// cells at `rect.left/top`, under image id `id`. The cursor is saved, moved to the
/// rect's top-left, the image transmitted+displayed in chunks, then restored — so it
/// never disturbs the TUI's cursor state.
///
/// Kitty requires the base64 payload split into ≤4096-byte chunks with `m=1` on
/// every chunk but the last. Pure string builder → no IO, unit-testable.
pub fn transmit_png_id(png: &[u8], rect: CellRect, id: u32) -> String {
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
            // (f=100), the id, and scale-to-cell-box (c=cols, r=rows).
            out.push_str(&format!(
                "\x1b_Ga=T,f=100,i={},c={},r={},m={};{}\x1b\\",
                id, rect.cols, rect.rows, m, chunk
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
    fn delete_image_by_id() {
        let s = delete_image(THUMB_ID_BASE + 2);
        assert_eq!(s, format!("\x1b_Ga=d,d=i,i={}\x1b\\", THUMB_ID_BASE + 2));
    }

    #[test]
    fn transmit_data_stores_without_display() {
        // >4096 base64 → 2+ chunks; first carries a=t, last has m=0.
        let png = vec![0u8; 5000];
        let s = transmit_data(&png, 101);
        assert!(s.contains("\x1b_Ga=t,i=101,f=100,q=2,m=1;"));
        assert!(s.contains("\x1b_Gm=0,q=2;")); // final chunk
        assert!(!s.contains("a=T")); // never the display action
        assert!(!s.contains("\x1b7")); // no cursor save (not displayed)
    }

    #[test]
    fn place_and_delete_placement_forms() {
        let rect = CellRect { left: 3, top: 4, cols: 8, rows: 5, pixel_width: None, pixel_height: None };
        let p = place(101, 2, rect);
        assert!(p.starts_with("\x1b7\x1b[5;4H")); // save + move to 1-based (row 5, col 4)
        assert!(p.contains("a=p,i=101,p=2,c=8,r=5,C=1,q=2"));
        assert!(p.ends_with("\x1b8")); // restore cursor
        assert_eq!(delete_placement(101, 2), "\x1b_Ga=d,d=i,i=101,p=2,q=2\x1b\\");
        assert_eq!(free_image(101), "\x1b_Ga=d,d=I,i=101,q=2\x1b\\");
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
