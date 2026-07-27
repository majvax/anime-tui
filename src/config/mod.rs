//! User configuration (TOML) plus resolved application paths. Defaults are
//! sensible so the app runs with no config file present.

use crate::errors::{Error, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Provider host, injected into the provider layer (never hard-coded there).
    pub base_url: String,
    /// mpv binary name/path. Spawned with an argument array, never via a shell.
    pub mpv_path: String,
    /// Opt in to embedded in-terminal (Kitty) playback. Default is the standalone
    /// mpv window, which is higher quality and has no terminal image-cache RAM.
    /// Embedded is only used when this is true AND the terminal supports Kitty
    /// graphics; otherwise the window backend is used.
    pub embedded_player: bool,
    /// Where posters/metadata/HLS segments are cached.
    pub cache_dir: Option<PathBuf>,
    /// Seconds between periodic playback-progress saves.
    pub progress_save_interval_secs: u64,
    pub network: Network,
    pub playback: Playback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Network {
    pub timeout_secs: u64,
    pub max_retries: u8,
    pub user_agent: String,
}

/// Embedded-playback tuning. The buffer caps bound mpv's own read-ahead so it
/// doesn't hold large amounts of the stream in RAM.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Playback {
    /// Max forward demuxer cache in MiB (mpv `--demuxer-max-bytes`).
    pub max_buffer_mib: u64,
    /// Seconds of stream to read ahead (mpv `--demuxer-readahead-secs`/`--cache-secs`).
    pub readahead_secs: u64,
    /// How many seconds the `i` key jumps to skip an opening. Typical anime OP ≈ 90 s.
    pub skip_intro_secs: u64,
    /// Preferred source when playing directly (Enter) without the picker. Matched
    /// against source labels like "vidmoly (VF)" by tokens (host + language), case-
    /// insensitively; if none matches, the most reliable available source is used.
    pub default_source: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: "https://nakanime.tv".into(),
            mpv_path: "mpv".into(),
            embedded_player: false,
            cache_dir: None,
            progress_save_interval_secs: 10,
            network: Network::default(),
            playback: Playback::default(),
        }
    }
}

impl Default for Network {
    fn default() -> Self {
        Self {
            timeout_secs: 20,
            max_retries: 2,
            user_agent: concat!("anime-tui/", env!("CARGO_PKG_VERSION")).into(),
        }
    }
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            max_buffer_mib: 64,
            readahead_secs: 10,
            skip_intro_secs: 85,
            default_source: "vidmoly (VF)".into(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::config_path()?)
    }

    /// Load config from a specific path. A missing file yields defaults (so the app
    /// runs out of the box); a present-but-invalid file is an error.
    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(|e| Error::Config(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    pub fn config_path() -> Result<PathBuf> {
        Ok(Self::dirs()?.config_dir().join("config.toml"))
    }

    pub fn cache_dir(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.cache_dir {
            return Ok(dir.clone());
        }
        Ok(Self::dirs()?.cache_dir().to_path_buf())
    }

    pub fn data_dir() -> Result<PathBuf> {
        Ok(Self::dirs()?.data_dir().to_path_buf())
    }

    fn dirs() -> Result<ProjectDirs> {
        ProjectDirs::from("dev", "anime-tui", "anime-tui")
            .ok_or_else(|| Error::Config("cannot determine platform directories".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.playback.skip_intro_secs, 85);
        assert!(!c.embedded_player);
    }

    #[test]
    fn partial_toml_keeps_defaults() {
        // Only overriding one playback field keeps the rest at defaults.
        let c: Config = toml::from_str("[playback]\nskip_intro_secs = 90\n").unwrap();
        assert_eq!(c.playback.skip_intro_secs, 90);
        assert_eq!(c.playback.max_buffer_mib, 64);
    }

    #[test]
    fn load_from_reads_alternate_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alt.toml");
        std::fs::write(&path, "base_url = \"https://example.test\"\n").unwrap();
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.base_url, "https://example.test");
        // A missing file falls back to defaults rather than erroring.
        let missing = Config::load_from(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(missing.base_url, Config::default().base_url);
    }
}
