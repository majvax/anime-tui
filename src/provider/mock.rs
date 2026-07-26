//! In-memory provider used for development and tests. No network access.

use crate::errors::Result;
use crate::models::*;
use async_trait::async_trait;

use super::Provider;

#[derive(Default)]
pub struct MockProvider;

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn search(&self, query: &str) -> Result<Vec<AnimeSummary>> {
        Ok(vec![AnimeSummary {
            id: AnimeId("mock-1".into()),
            title: format!("Result for {query}"),
            poster_url: None,
            year: Some(2024),
        }])
    }

    async fn details(&self, id: &AnimeId) -> Result<AnimeDetails> {
        Ok(AnimeDetails {
            id: id.clone(),
            title: "Mock Anime".into(),
            description: Some("A fixture title used for offline development.".into()),
            poster_url: None,
            genres: vec!["Action".into(), "Fantasy".into()],
            status: Some("Completed".into()),
            episodes: self.episodes(id).await?,
        })
    }

    async fn episodes(&self, _id: &AnimeId) -> Result<Vec<Episode>> {
        Ok((1..=12)
            .map(|n| Episode {
                id: EpisodeId(format!("ep-{n}")),
                number: n.to_string(),
                title: Some(format!("Episode {n}")),
                season_id: None,
            })
            .collect())
    }

    async fn resolve(&self, _anime: &AnimeId, _episode: &EpisodeId) -> Result<Vec<PlayableSource>> {
        // Points at a local test asset produced by scripts/gen_test_media.sh.
        Ok(vec![PlayableSource {
            url: "file:///tmp/anime-tui-poc-test.mp4".into(),
            quality: Quality::P720,
            label: Some("mock (local)".into()),
            http_headers: vec![],
            subtitles: vec![],
        }])
    }
}
