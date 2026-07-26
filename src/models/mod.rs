//! Provider-agnostic domain types shared across the app, UI, and persistence.
//! Nothing here is specific to Nakanime.

use serde::{Deserialize, Serialize};

/// Stable identifier for a title within a provider (e.g. a slug or numeric id).
/// Kept opaque so different providers can use different id schemes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AnimeId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodeId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimeSummary {
    pub id: AnimeId,
    pub title: String,
    pub poster_url: Option<String>,
    pub year: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimeDetails {
    pub id: AnimeId,
    pub title: String,
    pub description: Option<String>,
    pub poster_url: Option<String>,
    pub genres: Vec<String>,
    pub status: Option<String>,
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: EpisodeId,
    /// Human-facing episode number/label (may be non-numeric, e.g. "OVA 1").
    pub number: String,
    pub title: Option<String>,
    /// Provider-specific season identifier, used for grouping/sorting.
    #[serde(default)]
    pub season_id: Option<u32>,
}

/// A concrete, playable stream returned by the resolver after source resolution.
#[derive(Debug, Clone)]
pub struct PlayableSource {
    pub url: String,
    pub quality: Quality,
    /// Human-readable label shown in the source-selection list (e.g. "vidmoly (VF)").
    pub label: Option<String>,
    /// Headers mpv must send to fetch the stream (referer, user-agent, cookies).
    /// Treated as sensitive: never logged verbatim.
    pub http_headers: Vec<(String, String)>,
    pub subtitles: Vec<SubtitleTrack>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Quality {
    P360,
    P480,
    P720,
    P1080,
    Unknown,
}

impl Quality {
    pub fn label(self) -> &'static str {
        match self {
            Quality::P360 => "360p",
            Quality::P480 => "480p",
            Quality::P720 => "720p",
            Quality::P1080 => "1080p",
            Quality::Unknown => "auto",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub language: String,
    pub url: String,
    pub is_default: bool,
}
