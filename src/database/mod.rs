//! Local persistence: history, favourites, resume positions.
//! Uses SQLite (rusqlite, bundled) with atomic upserts.

use crate::errors::{Error, Result};
use crate::models::AnimeSummary;
use crate::models::AnimeId;
use rusqlite::Connection;
use std::path::Path;

pub struct Database {
    conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS anime_cache (
    provider   TEXT NOT NULL,
    anime_id   TEXT NOT NULL,
    title      TEXT NOT NULL,
    year       INTEGER,
    poster_url TEXT,
    PRIMARY KEY (provider, anime_id)
);

CREATE TABLE IF NOT EXISTS favourites (
    provider   TEXT NOT NULL,
    anime_id   TEXT NOT NULL,
    title      TEXT NOT NULL,
    added_at   INTEGER NOT NULL,
    PRIMARY KEY (provider, anime_id)
);

CREATE TABLE IF NOT EXISTS history (
    provider    TEXT NOT NULL,
    anime_id    TEXT NOT NULL,
    episode_id  TEXT NOT NULL,
    position_s  REAL NOT NULL DEFAULT 0,
    duration_s  REAL NOT NULL DEFAULT 0,
    watched_at  INTEGER NOT NULL,
    PRIMARY KEY (provider, anime_id, episode_id)
);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).map_err(|e| Error::Database(e.to_string()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| Error::Database(e.to_string()))?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)
            .map_err(|e| Error::Database(e.to_string()))?;
        // Migrate pre-existing databases created before `poster_url` existed. The
        // column is part of SCHEMA for fresh DBs; here we add it if missing and
        // ignore the "duplicate column name" error when it's already present.
        if let Err(e) = conn.execute("ALTER TABLE anime_cache ADD COLUMN poster_url TEXT", []) {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(Error::Database(msg));
            }
        }
        Ok(Self { conn })
    }

    /// Cache anime metadata so history/favourites can show titles and cover
    /// thumbnails. `poster_url` is preserved across upserts if a later call passes
    /// `None` (so a title-only refresh never wipes a known cover).
    pub fn cache_anime(
        &self,
        provider: &str,
        anime_id: &str,
        title: &str,
        year: Option<u16>,
        poster_url: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO anime_cache (provider, anime_id, title, year, poster_url)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(provider, anime_id) DO UPDATE SET
                 title = excluded.title,
                 year = excluded.year,
                 poster_url = COALESCE(excluded.poster_url, anime_cache.poster_url)",
            rusqlite::params![provider, anime_id, title, year.map(|y| y as i64), poster_url],
        ).map_err(|e| Error::Database(e.to_string()))?;
        Ok(())
    }

    /// Record/update resume position. Atomic upsert.
    pub fn save_progress(
        &self,
        provider: &str,
        anime: &str,
        episode: &str,
        position_s: f64,
        duration_s: f64,
    ) -> Result<()> {
        let now = now_unix();
        self.conn
            .execute(
                "INSERT INTO history (provider, anime_id, episode_id, position_s, duration_s, watched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(provider, anime_id, episode_id)
                 DO UPDATE SET position_s = excluded.position_s,
                               duration_s = excluded.duration_s,
                               watched_at = excluded.watched_at",
                rusqlite::params![provider, anime, episode, position_s, duration_s, now],
            )
            .map_err(|e| Error::Database(e.to_string()))?;
        Ok(())
    }

    /// Resume position (seconds) for an episode, if any.
    pub fn resume_position(&self, provider: &str, anime: &str, episode: &str) -> Result<Option<f64>> {
        self.conn
            .query_row(
                "SELECT position_s FROM history WHERE provider=?1 AND anime_id=?2 AND episode_id=?3",
                rusqlite::params![provider, anime, episode],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(Error::Database(other.to_string())),
            })
    }

    /// The episode id most recently watched for an anime, if any. Used to warm
    /// the resume episode (and the one after it) for instant playback.
    pub fn last_watched_episode(&self, provider: &str, anime: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT episode_id FROM history WHERE provider=?1 AND anime_id=?2 \
                 ORDER BY watched_at DESC LIMIT 1",
                rusqlite::params![provider, anime],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(Error::Database(other.to_string())),
            })
    }

    /// Resume positions for every episode in a given anime.
    pub fn resume_positions_for_anime(
        &self,
        provider: &str,
        anime: &str,
    ) -> Result<std::collections::HashMap<String, f64>> {
        let mut stmt = self.conn.prepare(
            "SELECT episode_id, position_s FROM history WHERE provider=?1 AND anime_id=?2 AND position_s > 0",
        ).map_err(|e| Error::Database(e.to_string()))?;
        let rows = stmt.query_map(rusqlite::params![provider, anime], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        }).map_err(|e| Error::Database(e.to_string()))?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (ep_id, pos) = row.map_err(|e| Error::Database(e.to_string()))?;
            map.insert(ep_id, pos);
        }
        Ok(map)
    }

    /// Add or remove an anime from favourites. Returns the new state (true = now a favourite).
    pub fn toggle_favourite(&self, provider: &str, anime_id: &str, title: &str) -> Result<bool> {
        let exists = self.is_favourite(provider, anime_id)?;
        if exists {
            self.conn.execute(
                "DELETE FROM favourites WHERE provider=?1 AND anime_id=?2",
                rusqlite::params![provider, anime_id],
            ).map_err(|e| Error::Database(e.to_string()))?;
            Ok(false)
        } else {
            self.conn.execute(
                "INSERT INTO favourites (provider, anime_id, title, added_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(provider, anime_id) DO UPDATE SET title = excluded.title, added_at = excluded.added_at",
                rusqlite::params![provider, anime_id, title, now_unix()],
            ).map_err(|e| Error::Database(e.to_string()))?;
            Ok(true)
        }
    }

    pub fn is_favourite(&self, provider: &str, anime_id: &str) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT 1 FROM favourites WHERE provider=?1 AND anime_id=?2",
                rusqlite::params![provider, anime_id],
                |_| Ok(()),
            )
            .map(|_| true)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(false),
                other => Err(Error::Database(other.to_string())),
            })
    }

    pub fn list_favourites(&self, provider: &str) -> Result<Vec<AnimeSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.anime_id, f.title, ac.year, ac.poster_url
             FROM favourites f
             LEFT JOIN anime_cache ac ON f.provider = ac.provider AND f.anime_id = ac.anime_id
             WHERE f.provider = ?1
             ORDER BY f.added_at DESC, f.rowid DESC",
        ).map_err(|e| Error::Database(e.to_string()))?;
        let rows = stmt.query_map(rusqlite::params![provider], |row| {
            Ok(AnimeSummary {
                id: AnimeId(row.get::<_, String>(0)?),
                title: row.get(1)?,
                poster_url: row.get::<_, Option<String>>(3)?,
                year: row.get::<_, Option<i64>>(2)?.map(|y| y as u16),
            })
        }).map_err(|e| Error::Database(e.to_string()))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::Database(e.to_string()))
    }

    pub fn list_history(&self, provider: &str) -> Result<Vec<AnimeSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT h.anime_id, COALESCE(ac.title, h.anime_id), ac.year, ac.poster_url
             FROM history h
             LEFT JOIN anime_cache ac ON ac.provider=h.provider AND ac.anime_id=h.anime_id
             WHERE h.provider=?1
               AND h.rowid IN (SELECT MAX(rowid) FROM history WHERE provider=?1 GROUP BY anime_id)
             ORDER BY h.watched_at DESC, h.rowid DESC
             LIMIT 100",
        ).map_err(|e| Error::Database(e.to_string()))?;
        let rows = stmt.query_map(rusqlite::params![provider], |row| {
            Ok(AnimeSummary {
                id: AnimeId(row.get::<_, String>(0)?),
                title: row.get(1)?,
                poster_url: row.get::<_, Option<String>>(3)?,
                year: row.get::<_, Option<i64>>(2)?.map(|y| y as u16),
            })
        }).map_err(|e| Error::Database(e.to_string()))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::Database(e.to_string()))
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_roundtrip_and_upsert() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.resume_position("mock", "a", "e1").unwrap(), None);

        db.save_progress("mock", "a", "e1", 42.0, 1400.0).unwrap();
        assert_eq!(db.resume_position("mock", "a", "e1").unwrap(), Some(42.0));

        db.save_progress("mock", "a", "e1", 99.5, 1400.0).unwrap();
        assert_eq!(db.resume_position("mock", "a", "e1").unwrap(), Some(99.5));
    }

    #[test]
    fn last_watched_episode_returns_watched() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.last_watched_episode("mock", "a").unwrap(), None);
        db.save_progress("mock", "a", "e1", 10.0, 100.0).unwrap();
        assert_eq!(db.last_watched_episode("mock", "a").unwrap(), Some("e1".into()));
        // Scoped per anime.
        assert_eq!(db.last_watched_episode("mock", "other").unwrap(), None);
    }

    #[test]
    fn toggle_favourite_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        assert!(!db.is_favourite("mock", "a1").unwrap());

        let now_fav = db.toggle_favourite("mock", "a1", "Anime A").unwrap();
        assert!(now_fav);
        assert!(db.is_favourite("mock", "a1").unwrap());

        let now_fav = db.toggle_favourite("mock", "a1", "Anime A").unwrap();
        assert!(!now_fav);
        assert!(!db.is_favourite("mock", "a1").unwrap());
    }

    #[test]
    fn list_favourites_ordered_by_recency() {
        let db = Database::open_in_memory().unwrap();
        db.toggle_favourite("mock", "a1", "First").unwrap();
        db.toggle_favourite("mock", "a2", "Second").unwrap();
        let favs = db.list_favourites("mock").unwrap();
        assert_eq!(favs.len(), 2);
        assert_eq!(favs[0].title, "Second");
    }

    #[test]
    fn list_history_distinct_by_anime() {
        let db = Database::open_in_memory().unwrap();
        db.cache_anime("mock", "a1", "Anime One", Some(2020), None).unwrap();
        db.save_progress("mock", "a1", "ep1", 10.0, 1400.0).unwrap();
        db.save_progress("mock", "a1", "ep2", 20.0, 1400.0).unwrap();
        let hist = db.list_history("mock").unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].title, "Anime One");
        assert_eq!(hist[0].year, Some(2020));
    }

    #[test]
    fn cached_poster_url_surfaces_in_lists() {
        let db = Database::open_in_memory().unwrap();
        db.cache_anime("mock", "a1", "Anime One", Some(2020), Some("https://img/p.jpg"))
            .unwrap();
        db.toggle_favourite("mock", "a1", "Anime One").unwrap();
        db.save_progress("mock", "a1", "ep1", 10.0, 1400.0).unwrap();

        let favs = db.list_favourites("mock").unwrap();
        assert_eq!(favs[0].poster_url.as_deref(), Some("https://img/p.jpg"));
        let hist = db.list_history("mock").unwrap();
        assert_eq!(hist[0].poster_url.as_deref(), Some("https://img/p.jpg"));
    }

    #[test]
    fn cache_anime_preserves_poster_on_title_only_update() {
        let db = Database::open_in_memory().unwrap();
        db.cache_anime("mock", "a1", "One", None, Some("https://img/p.jpg")).unwrap();
        // A later title-only refresh (poster None) must not wipe the known cover.
        db.cache_anime("mock", "a1", "One (updated)", None, None).unwrap();
        db.toggle_favourite("mock", "a1", "One").unwrap();
        let favs = db.list_favourites("mock").unwrap();
        assert_eq!(favs[0].poster_url.as_deref(), Some("https://img/p.jpg"));
    }

    #[test]
    fn resume_positions_for_anime() {
        let db = Database::open_in_memory().unwrap();
        db.save_progress("mock", "a1", "ep1", 42.0, 1400.0).unwrap();
        db.save_progress("mock", "a1", "ep2", 0.0, 1400.0).unwrap();
        let pos = db.resume_positions_for_anime("mock", "a1").unwrap();
        assert_eq!(pos.get("ep1"), Some(&42.0));
        assert!(!pos.contains_key("ep2")); // position_s == 0 excluded
    }
}
