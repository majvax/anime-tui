//! anime-tui entry point: load config, pick a provider and playback backend,
//! open the local database, and run the interactive TUI event loop.

use std::sync::Arc;

use anime_tui::app::run::Runner;
use anime_tui::config::Config;
use anime_tui::database::Database;
use anime_tui::player;
use anime_tui::provider::{mock::MockProvider, nakanime::Nakanime, Provider};

#[tokio::main]
async fn main() {
    // Logs go to stderr and are captured to a file by the caller if desired;
    // never to stdout (that's the TUI surface).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "anime_tui=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    if let Err(e) = real_main().await {
        // TerminalGuard has restored the terminal via Drop by now.
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

async fn real_main() -> anime_tui::errors::Result<()> {
    let config = Config::load()?;

    // Use the real Nakanime provider by default. Set base_url = "" in config
    // to fall back to the offline mock (useful for UI development / no network).
    let provider: Arc<dyn Provider> = if config.base_url.is_empty() {
        Arc::new(MockProvider)
    } else {
        Arc::new(Nakanime::from_config(&config)?)
    };

    let backend = player::select_backend(&config);
    tracing::info!(?backend, provider = provider.name(), "starting");

    let db = Database::open(&Config::data_dir()?.join("anime-tui.sqlite3"))?;

    Runner::new(&config, provider, db)?.run().await
}
