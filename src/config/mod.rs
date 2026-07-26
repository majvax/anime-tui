//! User configuration (TOML) plus resolved application paths. Defaults are
//! sensible so the app runs with no config file present.

use crate::errors::{Error, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        match std::fs::read_to_string(&path) {
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
}
