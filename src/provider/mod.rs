//! Provider abstraction. Any anime source implements [`Provider`]; the rest of
//! the app is written against this trait so a second source can be added later.
//!
//! Source-*resolution* (turning an episode into a playable URL) lives in the
//! [`crate::resolver`] module; providers focus on catalogue/metadata.

pub mod nakanime;

use crate::errors::Result;
use crate::models::{
    AnimeDetails, AnimeId, AnimeSummary, CatalogPage, Episode, EpisodeId, PlayableSource,
};
use async_trait::async_trait;

#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable machine name, e.g. "nakanime". Used in the DB and cache keys.
    fn name(&self) -> &'static str;

    /// Fetch one page of catalogue results for `query` (empty = full catalogue),
    /// ordered by `sort` (a provider-validated value). Carries pagination metadata
    /// so the browse UI can page through and show counts.
    async fn search_page(&self, query: &str, page: u32, sort: &str) -> Result<CatalogPage>;

    /// Convenience: the first page's items with the default sort. Kept so simpler
    /// callers/tests need not thread pagination through.
    async fn search(&self, query: &str) -> Result<Vec<AnimeSummary>> {
        Ok(self.search_page(query, 1, "relevance").await?.items)
    }

    async fn details(&self, id: &AnimeId) -> Result<AnimeDetails>;

    async fn episodes(&self, id: &AnimeId) -> Result<Vec<Episode>>;

    /// Resolve a playable stream for an episode. Providers may delegate to
    /// [`crate::resolver`] internally. Returns sources best-quality-first.
    async fn resolve(&self, anime: &AnimeId, episode: &EpisodeId) -> Result<Vec<PlayableSource>>;
}

/// A mock provider backed by in-memory fixtures. Lets the whole UI/playback
/// stack be developed and tested (Phase 2) with zero network dependency.
pub mod mock;
