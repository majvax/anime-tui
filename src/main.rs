//! anime-tui entry point: load config, pick a provider and playback backend,
//! open the local database, and run the interactive TUI event loop.

use std::path::PathBuf;
use std::sync::Arc;

use anime_tui::app::run::Runner;
use anime_tui::config::Config;
use anime_tui::database::Database;
use anime_tui::player;
use anime_tui::provider::{mock::MockProvider, nakanime::Nakanime, Provider};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
anime-tui — terminal client for Nakanime with in-terminal (Kitty) playback via mpv

USAGE:
    anime-tui [OPTIONS]

OPTIONS:
    -h, --help           Print this help and exit
    -V, --version        Print version and exit
        --paths          Print config/data/cache directories and exit
        --config <PATH>  Use an alternate config file

Runtime deps: mpv and yt-dlp on PATH (ffmpeg too, for downloading HLS episodes).
Config is TOML at the path shown by --paths.";

/// Parsed command-line options. Kept tiny and dependency-free.
struct Cli {
    config: Option<PathBuf>,
}

/// Parse args, handling exit-early flags (`--help`/`--version`/`--paths`) directly.
fn parse_args() -> Cli {
    let mut config = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("anime-tui {VERSION}");
                std::process::exit(0);
            }
            "--paths" => {
                print_paths();
                std::process::exit(0);
            }
            "--config" => match args.next() {
                Some(p) => config = Some(PathBuf::from(p)),
                None => {
                    eprintln!("error: --config requires a path argument");
                    std::process::exit(2);
                }
            },
            other => {
                eprintln!("error: unknown argument '{other}'\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    Cli { config }
}

/// Print the resolved platform directories (best-effort; unknowns show as "?").
fn print_paths() {
    let show = |label: &str, r: anime_tui::errors::Result<PathBuf>| {
        println!("{label}: {}", r.map(|p| p.display().to_string()).unwrap_or_else(|_| "?".into()));
    };
    show("config", Config::config_path());
    show("data", Config::data_dir());
    show("cache", Config::default().cache_dir());
}

#[tokio::main]
async fn main() {
    let cli = parse_args();

    // Logs go to stderr and are captured to a file by the caller if desired;
    // never to stdout (that's the TUI surface).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "anime_tui=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    if let Err(e) = real_main(cli).await {
        // TerminalGuard has restored the terminal via Drop by now.
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

async fn real_main(cli: Cli) -> anime_tui::errors::Result<()> {
    let config = match cli.config {
        Some(path) => Config::load_from(&path)?,
        None => Config::load()?,
    };

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
