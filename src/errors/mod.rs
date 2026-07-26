//! Structured error types. The application uses `Result<T>` everywhere and
//! avoids panics on any recoverable path (network, provider, IO, playback).

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error: {0}")]
    Network(String),

    #[error("request timed out")]
    Timeout,

    #[error("provider error: {0}")]
    Provider(String),

    /// The provider's HTML/JSON no longer matches our parsers. This is the
    /// actionable signal a maintainer needs when Nakanime changes its markup.
    #[error("provider layout changed while parsing {context}: {detail}")]
    ProviderChanged { context: String, detail: String },

    #[error("could not resolve a playable source: {0}")]
    Resolve(String),

    #[error("invalid or unsupported url: {0}")]
    InvalidUrl(String),

    #[error("player error: {0}")]
    Player(String),

    #[error("database error: {0}")]
    Database(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("feature not implemented yet: {0}")]
    NotImplemented(&'static str),
}

impl Error {
    /// True for errors where a bounded retry is sensible (transient network).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Network(_) | Error::Timeout)
    }
}

/// Small helper so provider parsers can flag layout drift ergonomically.
pub fn provider_changed(context: impl fmt::Display, detail: impl fmt::Display) -> Error {
    Error::ProviderChanged {
        context: context.to_string(),
        detail: detail.to_string(),
    }
}
