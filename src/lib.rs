//! anime-tui library crate. See `docs/ARCHITECTURE.md` for the big picture.
//!
//! Module map:
//!   app       — application state + typed view/navigation state machine
//!   ui        — ratatui rendering (reserves the video rect, never overpaints it)
//!   input     — key events -> typed Actions (configurable bindings)
//!   provider  — Provider trait + mock; provider::nakanime (isolated selectors)
//!   resolver  — episode -> validated playable URL (URL/scheme allowlist)
//!   player    — mpv-over-IPC control; player::kitty + player::mpv backends
//!   database  — SQLite history/favourites/resume (atomic upserts)
//!   cache     — poster/metadata cache with filename sanitisation
//!   config    — TOML config + platform paths
//!   models    — provider-agnostic domain types
//!   errors    — structured Error/Result, no panics on recoverable paths

pub mod app;
pub mod cache;
pub mod config;
pub mod database;
pub mod errors;
pub mod input;
pub mod models;
pub mod player;
pub mod provider;
pub mod resolver;
pub mod terminal;
pub mod ui;
