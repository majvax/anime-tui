//! Dev-only: hit the real Nakanime provider and save one embed page per distinct
//! host to /tmp/anime-tui-dump/, so host resolvers can be built/verified from real
//! samples. Run: `cargo run --example dump_sources`.

use anime_tui::config::Config;
use anime_tui::provider::nakanime::Nakanime;
use anime_tui::provider::Provider;
use std::collections::HashMap;

fn host_key(label: &str, url: &str) -> String {
    let l = label.to_lowercase();
    for h in ["voe", "sibnet", "vidmoly", "ok.ru", "mail", "smoothpre", "filemoon", "vidzy", "luluvdo", "lulustream"] {
        if l.contains(h) || url.contains(h) {
            return h.to_string();
        }
    }
    l.split_whitespace().next().unwrap_or("other").to_string()
}

#[tokio::main]
async fn main() {
    let config = Config::load().expect("config");
    let provider = Nakanime::from_config(&config).expect("provider");
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
        .build()
        .unwrap();
    let dir = std::path::Path::new("/tmp/anime-tui-dump");
    std::fs::create_dir_all(dir).unwrap();

    let mut saved: HashMap<String, String> = HashMap::new();
    let results = provider.search("").await.expect("catalogue");
    for anime in results.iter().take(30) {
        let Ok(details) = provider.details(&anime.id).await else { continue };
        for ep in details.episodes.iter().take(1) {
            let Ok(sources) = provider.resolve(&anime.id, &ep.id).await else { continue };
            for s in &sources {
                let label = s.label.clone().unwrap_or_default();
                let key = host_key(&label, &s.url);
                if saved.contains_key(&key) {
                    continue;
                }
                if let Ok(resp) = client.get(&s.url).header("Referer", "https://nakanime.tv/").send().await {
                    let status = resp.status();
                    if let Ok(body) = resp.text().await {
                        let path = dir.join(format!("{key}.html"));
                        std::fs::write(&path, &body).unwrap();
                        println!("SAVED {key:12} status {status} bytes {:7}  <- {}", body.len(), s.url);
                        saved.insert(key, s.url.clone());
                    }
                }
            }
        }
    }
    println!("\nhosts captured: {:?}", saved.keys().collect::<Vec<_>>());
}
