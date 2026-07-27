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

    async fn search_page(&self, query: &str, page: u32, _sort: &str) -> Result<CatalogPage> {
        // Synthetic catalogue with two pages so offline dev exercises pagination.
        const TOTAL: u32 = 40;
        const PER_PAGE: u32 = 32;
        let total_pages = TOTAL.div_ceil(PER_PAGE);
        let page = page.clamp(1, total_pages);
        let start = (page - 1) * PER_PAGE;
        let end = (start + PER_PAGE).min(TOTAL);
        let label = if query.is_empty() { "Catalogue".to_string() } else { format!("Result for {query}") };
        let items = (start..end)
            .map(|i| AnimeSummary {
                id: AnimeId(format!("mock-{}", i + 1)),
                title: format!("{label} #{}", i + 1),
                poster_url: None,
                year: Some(2024 - (i % 10) as u16),
            })
            .collect();
        Ok(CatalogPage { items, page, total_pages, total: TOTAL })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn search_page_paginates() {
        let p = MockProvider;
        let first = p.search_page("", 1, "relevance").await.unwrap();
        assert_eq!(first.page, 1);
        assert_eq!(first.total_pages, 2);
        assert_eq!(first.total, 40);
        assert_eq!(first.items.len(), 32);
        let second = p.search_page("", 2, "relevance").await.unwrap();
        assert_eq!(second.page, 2);
        assert_eq!(second.items.len(), 8);
        // Pages don't overlap.
        assert_ne!(first.items[0].id.0, second.items[0].id.0);
    }
}
