//! Episode chapters. Stored chapters come from Podcasting 2.0 JSON
//! (`<podcast:chapters>`); the playback engine falls back to chapters
//! embedded in the audio file when none are stored. GStreamer's id3demux
//! doesn't parse ID3 CHAP frames, so MP3 chapters are read here.

use rusqlite::{params, Connection};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub start_ms: u64,
    pub title: String,
    pub image_url: Option<String>,
    pub url: Option<String>,
}

#[derive(Deserialize)]
struct JsonChapters {
    chapters: Vec<JsonChapter>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonChapter {
    start_time: f64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    img: Option<String>,
    #[serde(default)]
    url: Option<String>,
    /// `false` marks a silent chapter (artwork or link change only) that
    /// is left out of the table of contents.
    #[serde(default = "default_true")]
    toc: bool,
}

fn default_true() -> bool {
    true
}

/// Parses a Podcasting 2.0 chapters file. Silent (`toc: false`) and
/// negative-start entries are dropped; the result is sorted by start.
pub fn parse_json(text: &str) -> Result<Vec<Chapter>, serde_json::Error> {
    let parsed: JsonChapters = serde_json::from_str(text)?;
    let mut chapters: Vec<Chapter> = parsed
        .chapters
        .into_iter()
        .filter(|c| c.toc && c.start_time >= 0.0 && c.start_time.is_finite())
        .map(|c| Chapter {
            start_ms: (c.start_time * 1000.0).round() as u64,
            title: c.title.unwrap_or_default(),
            image_url: c.img,
            url: c.url,
        })
        .collect();
    chapters.sort_by_key(|c| c.start_ms);
    Ok(chapters)
}

/// ID3 CHAP frames, ordered by start. Empty when the file has no tag or
/// no chapters.
pub fn from_id3_file(path: &std::path::Path) -> Vec<Chapter> {
    let Ok(tag) = id3::Tag::read_from_path(path) else { return Vec::new() };
    let mut chapters: Vec<Chapter> = tag
        .chapters()
        .map(|c| Chapter {
            start_ms: u64::from(c.start_time),
            title: c
                .frames
                .iter()
                .find(|f| f.id() == "TIT2")
                .and_then(|f| f.content().text())
                .unwrap_or_default()
                .to_string(),
            image_url: None,
            url: c.frames.iter().find_map(|f| f.content().link().map(str::to_string)),
        })
        .collect();
    chapters.sort_by_key(|c| c.start_ms);
    chapters
}

pub fn replace(conn: &Connection, episode_id: i64, chapters: &[Chapter]) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM chapters WHERE episode_id = ?1", [episode_id])?;
    for c in chapters {
        tx.execute(
            "INSERT INTO chapters (episode_id, start_ms, title, image_url, url) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![episode_id, c.start_ms as i64, c.title, c.image_url, c.url],
        )?;
    }
    tx.commit()
}

pub fn list(conn: &Connection, episode_id: i64) -> rusqlite::Result<Vec<Chapter>> {
    let mut stmt =
        conn.prepare("SELECT start_ms, title, image_url, url FROM chapters WHERE episode_id = ?1 ORDER BY start_ms")?;
    let rows = stmt.query_map([episode_id], |row| {
        Ok(Chapter {
            start_ms: row.get::<_, i64>(0)?.max(0) as u64,
            title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            image_url: row.get(2)?,
            url: row.get(3)?,
        })
    })?;
    rows.collect()
}

/// Episodes that have a chapters URL and no stored chapters.
pub fn missing_chapters_url(conn: &Connection, episode_id: i64) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT chapters_url FROM episodes
         WHERE id = ?1 AND chapters_url IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM chapters WHERE episode_id = ?1)",
        [episode_id],
        |row| row.get(0),
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_podcasting_2_chapters_and_drops_silent_ones() {
        let json = r#"{
            "version": "1.2.0",
            "chapters": [
                {"startTime": 125.5, "title": "News", "url": "https://example.com"},
                {"startTime": 0, "title": "Intro", "img": "https://example.com/a.jpg"},
                {"startTime": 60, "title": "Art change", "toc": false},
                {"startTime": 300}
            ]
        }"#;
        let chapters = parse_json(json).unwrap();
        let starts: Vec<u64> = chapters.iter().map(|c| c.start_ms).collect();
        assert_eq!(starts, vec![0, 125_500, 300_000]);
        assert_eq!(chapters[0].image_url.as_deref(), Some("https://example.com/a.jpg"));
        assert_eq!(chapters[2].title, "");
    }

    #[test]
    fn replace_and_list_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open_connection(&dir.path().join("t.db")).unwrap();
        crate::db::migrate(&conn).unwrap();
        conn.execute("INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('u', 't', 1)", []).unwrap();
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, chapters_url)
             VALUES (1, 'g', 't', 'a', '2026-01-01T00:00:00Z', 'https://example.com/c.json')",
            [],
        )
        .unwrap();
        assert_eq!(missing_chapters_url(&conn, 1).unwrap().as_deref(), Some("https://example.com/c.json"));
        let chapters = vec![
            Chapter { start_ms: 0, title: "A".into(), image_url: None, url: None },
            Chapter { start_ms: 5000, title: "B".into(), image_url: None, url: None },
        ];
        replace(&conn, 1, &chapters).unwrap();
        replace(&conn, 1, &chapters).unwrap();
        assert_eq!(list(&conn, 1).unwrap(), chapters);
        assert_eq!(missing_chapters_url(&conn, 1).unwrap(), None);
    }
}
