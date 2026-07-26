//! Provider abstraction. Any anime source implements [`Provider`]; the rest of
//! the app is written against this trait so a second source can be added later.
//!
//! Source-*resolution* (turning an episode into a playable URL) lives in the
//! [`crate::resolver`] module; providers focus on catalogue/metadata.

pub mod nakanime;

use crate::errors::Result;
use crate::models::{AnimeDetails, AnimeId, AnimeSummary, Episode, EpisodeId, PlayableSource};
use async_trait::async_trait;

#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable machine name, e.g. "nakanime". Used in the DB and cache keys.
    fn name(&self) -> &'static str;

    async fn search(&self, query: &str) -> Result<Vec<AnimeSummary>>;

    async fn details(&self, id: &AnimeId) -> Result<AnimeDetails>;

    async fn episodes(&self, id: &AnimeId) -> Result<Vec<Episode>>;

    /// Resolve a playable stream for an episode. Providers may delegate to
    /// [`crate::resolver`] internally. Returns sources best-quality-first.
    async fn resolve(&self, anime: &AnimeId, episode: &EpisodeId) -> Result<Vec<PlayableSource>>;
}

/// A mock provider backed by in-memory fixtures. Lets the whole UI/playback
/// stack be developed and tested (Phase 2) with zero network dependency.
pub mod mock;
