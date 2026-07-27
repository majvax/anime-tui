//! Dev-only: hit the real Nakanime provider, list each episode's sources, and save
//! the embed pages for voe/sibnet/vidmoly so host resolvers can be built/verified
//! from real samples. Writes to /tmp/anime-tui-dump/. Run: `cargo run --example dump_voe`.

use anime_tui::config::Config;
use anime_tui::provider::nakanime::Nakanime;
use anime_tui::provider::Provider;

#[tokio::main]
async fn main() {
    let config = Config::load().expect("load config");
    let provider = Nakanime::from_config(&config).expect("build provider");
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
        .build()
        .unwrap();

    let dir = std::path::Path::new("/tmp/anime-tui-dump");
    std::fs::create_dir_all(dir).unwrap();

    let results = provider.search("").await.expect("catalogue search");
    println!("catalogue: {} titles", results.len());

    let want = ["voe", "sibnet", "vidmoly"];
    let mut saved: Vec<String> = Vec::new();

    'outer: for anime in results.iter().take(20) {
        let details = match provider.details(&anime.id).await {
            Ok(d) => d,
            Err(_) => continue,
        };
        for ep in details.episodes.iter().take(1) {
            let sources = match provider.resolve(&anime.id, &ep.id).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            for s in &sources {
                let label = s.label.clone().unwrap_or_default();
                println!("  [{}] {} -> {}", anime.title, label, s.url);
                let lc = label.to_lowercase();
                if let Some(host) = want.iter().find(|h| lc.contains(**h) || s.url.contains(**h)) {
                    if saved.iter().any(|h| h == host) {
                        continue;
                    }
                    if let Ok(resp) = client
                        .get(&s.url)
                        .header("Referer", "https://nakanime.tv/")
                        .send()
                        .await
                    {
                        let status = resp.status();
                        if let Ok(body) = resp.text().await {
                            let path = dir.join(format!("{host}.html"));
                            std::fs::write(&path, &body).unwrap();
                            println!(
                                "    SAVED {host} ({} bytes, status {status}) -> {}",
                                body.len(),
                                path.display()
                            );
                            saved.push(host.to_string());
                        }
                    }
                }
            }
            if want.iter().all(|h| saved.iter().any(|s| s == h)) {
                break 'outer;
            }
        }
    }
    println!("saved hosts: {saved:?}");
}
