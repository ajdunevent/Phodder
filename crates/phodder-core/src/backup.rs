//! Backups: every table as JSON rows, without audio, cached artwork or
//! passwords (those live in the keyring; a sync account restored on another
//! device asks for its password again).
//!
//! Restoring replaces everything. Rows are matched to the current schema by
//! column name, so a backup from an older version restores into a newer
//! one. A download survives when the restored episode (same feed URL and
//! GUID) had its file on this device; other queued episodes are downloaded
//! again and the rest are left not downloaded.

use rusqlite::types::Value;
use rusqlite::{params, Connection};
use serde_json::{Map, Value as Json};
use std::collections::HashMap;
use std::path::PathBuf;

pub const FORMAT: &str = "phodder-backup";
const FORMAT_VERSION: i64 = 1;

/// Parents before children, so rows insert with foreign keys intact.
const TABLES: [&str; 9] = ["groups", "subscriptions", "episodes", "value_recipients", "chapters", "sync_accounts", "sync_outbox", "boosts_sent", "settings"];

/// Device-specific columns left out of a backup.
const LOCAL_COLUMNS: [(&str, &str); 4] = [
    ("subscriptions", "artwork_local_path"),
    ("episodes", "artwork_local_path"),
    ("episodes", "local_file_path"),
    ("episodes", "download_progress"),
];

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("this isn't a Phodder backup")]
    NotABackup,
    #[error("the restored data doesn't hold together ({0} broken references)")]
    ForeignKeys(usize),
}

#[derive(Debug, Default, PartialEq)]
pub struct RestoreReport {
    pub podcasts: usize,
    pub episodes: usize,
    /// Downloads kept because the restored episode is the same one.
    pub downloads_kept: usize,
    /// Files of downloads that aren't part of the restored library.
    pub files_to_delete: Vec<PathBuf>,
}

pub fn export(conn: &Connection, app_version: &str) -> Result<String, BackupError> {
    let mut tables = Map::new();
    for table in TABLES {
        let columns = column_names(conn, table)?;
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table}"))?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut object = Map::new();
            for (i, column) in columns.iter().enumerate() {
                if LOCAL_COLUMNS.contains(&(table, column.as_str())) {
                    continue;
                }
                object.insert(column.clone(), to_json(row.get::<_, Value>(i)?));
            }
            out.push(Json::Object(object));
        }
        tables.insert(table.to_string(), Json::Array(out));
    }
    let backup = serde_json::json!({
        "format": FORMAT,
        "format_version": FORMAT_VERSION,
        "schema_version": crate::db::SCHEMA_VERSION,
        "app_version": app_version,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "tables": tables,
    });
    Ok(serde_json::to_string(&backup)?)
}

/// Checks a backup without changing anything; returns its podcast and
/// episode counts.
pub fn inspect(json: &str) -> Result<(usize, usize), BackupError> {
    let tables = parse(json)?;
    let count = |name: &str| tables.get(name).and_then(Json::as_array).map_or(0, Vec::len);
    Ok((count("subscriptions"), count("episodes")))
}

fn parse(json: &str) -> Result<Map<String, Json>, BackupError> {
    let mut backup: Json = serde_json::from_str(json).map_err(|_| BackupError::NotABackup)?;
    if backup.get("format").and_then(Json::as_str) != Some(FORMAT) {
        return Err(BackupError::NotABackup);
    }
    match backup.get_mut("tables").map(Json::take) {
        Some(Json::Object(tables)) => Ok(tables),
        _ => Err(BackupError::NotABackup),
    }
}

/// Replaces every table with the backup's rows, in one transaction. The
/// caller deletes `files_to_delete` once it has committed.
pub fn restore(conn: &Connection, json: &str) -> Result<RestoreReport, BackupError> {
    let tables = parse(json)?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = (|| {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let inner = (|| {
            let existing = existing_downloads(conn)?;
            for table in TABLES.iter().rev() {
                conn.execute(&format!("DELETE FROM {table}"), [])?;
            }
            for table in TABLES {
                let Some(rows) = tables.get(table).and_then(Json::as_array) else { continue };
                let columns = column_names(conn, table)?;
                for row in rows {
                    let Some(object) = row.as_object() else { continue };
                    let present: Vec<&String> = columns.iter().filter(|c| object.contains_key(c.as_str())).collect();
                    if present.is_empty() {
                        continue;
                    }
                    let sql = format!(
                        "INSERT INTO {table} ({}) VALUES ({})",
                        present.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", "),
                        (1..=present.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", "),
                    );
                    let values: Vec<Value> = present.iter().map(|c| from_json(&object[c.as_str()])).collect();
                    conn.execute(&sql, rusqlite::params_from_iter(values))?;
                }
            }
            let report = reconcile_downloads(conn, existing)?;
            let broken = conn.prepare("PRAGMA foreign_key_check")?.query_map([], |_| Ok(()))?.count();
            if broken > 0 {
                return Err(BackupError::ForeignKeys(broken));
            }
            Ok(report)
        })();
        match inner {
            Ok(report) => {
                conn.execute_batch("COMMIT")?;
                Ok(report)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    })();
    conn.pragma_update(None, "foreign_keys", "ON")?;
    result
}

/// Downloaded files on this device, by feed URL and GUID.
fn existing_downloads(conn: &Connection) -> rusqlite::Result<HashMap<(String, String), PathBuf>> {
    let mut stmt = conn.prepare(
        "SELECT s.feed_url, e.guid, e.local_file_path FROM episodes e JOIN subscriptions s ON s.id = e.subscription_id
         WHERE e.download_status = 'downloaded' AND e.local_file_path IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |row| Ok(((row.get(0)?, row.get(1)?), PathBuf::from(row.get::<_, String>(2)?))))?;
    rows.collect()
}

fn reconcile_downloads(conn: &Connection, mut existing: HashMap<(String, String), PathBuf>) -> Result<RestoreReport, BackupError> {
    let restored: Vec<(i64, String, String, String, bool)> = {
        let mut stmt = conn.prepare(
            "SELECT e.id, s.feed_url, e.guid, e.download_status, e.group_queue_position IS NOT NULL
             FROM episodes e JOIN subscriptions s ON s.id = e.subscription_id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut report = RestoreReport { episodes: restored.len(), ..Default::default() };
    for (id, feed_url, guid, status, queued) in restored {
        match existing.remove(&(feed_url, guid)).filter(|path| path.exists()) {
            Some(path) => {
                conn.execute(
                    "UPDATE episodes SET download_status = 'downloaded', local_file_path = ?1, download_progress = NULL WHERE id = ?2",
                    params![path.to_string_lossy(), id],
                )?;
                report.downloads_kept += 1;
            }
            None => {
                let new_status = if queued && status != "none" { "queued" } else { "none" };
                conn.execute(
                    "UPDATE episodes SET download_status = ?1, local_file_path = NULL, download_progress = NULL WHERE id = ?2",
                    params![new_status, id],
                )?;
            }
        }
    }
    conn.execute("UPDATE episodes SET artwork_local_path = NULL", [])?;
    conn.execute("UPDATE subscriptions SET artwork_local_path = NULL", [])?;
    report.podcasts = conn.query_row("SELECT COUNT(*) FROM subscriptions", [], |row| row.get(0))?;
    report.files_to_delete = existing.into_values().collect();
    Ok(report)
}

fn column_names(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    names.collect()
}

fn to_json(value: Value) -> Json {
    match value {
        Value::Null | Value::Blob(_) => Json::Null,
        Value::Integer(i) => Json::from(i),
        Value::Real(f) => serde_json::Number::from_f64(f).map_or(Json::Null, Json::Number),
        Value::Text(s) => Json::String(s),
    }
}

fn from_json(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Integer(i64::from(*b)),
        Json::Number(n) => n.as_i64().map_or_else(|| Value::Real(n.as_f64().unwrap_or(0.0)), Value::Integer),
        Json::String(s) => Value::Text(s.clone()),
        other => Value::Text(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library(dir: &std::path::Path) -> Connection {
        let conn = crate::open_connection(&dir.join("test.db")).unwrap();
        crate::db::migrate(&conn).unwrap();
        conn
    }

    fn add_episode(conn: &Connection, sub: i64, guid: &str, status: &str, path: Option<&std::path::Path>, queued: bool) -> i64 {
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, download_status, local_file_path, group_queue_position, playback_position_ms)
             VALUES (?1, ?2, ?2, 'https://example.com/a.mp3', '2026-01-01T00:00:00Z', ?3, ?4, ?5, 1234)",
            params![sub, guid, status, path.map(|p| p.to_string_lossy().to_string()), queued.then_some(1.0)],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn a_backup_restores_the_library_and_keeps_matching_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let conn = library(dir.path());
        conn.execute("INSERT INTO subscriptions (feed_url, title, group_id, playback_rate, artwork_local_path) VALUES ('https://example.com/feed', 'Show', 1, 1.5, '/art/x.jpg')", [])
            .unwrap();
        let sub = conn.last_insert_rowid();
        let kept = dir.path().join("kept.mp3");
        std::fs::write(&kept, b"audio").unwrap();
        add_episode(&conn, sub, "kept", "downloaded", Some(&kept), true);
        add_episode(&conn, sub, "queued-elsewhere", "downloaded", Some(&dir.path().join("gone.mp3")), true);
        add_episode(&conn, sub, "library-only", "downloaded", Some(&dir.path().join("gone2.mp3")), false);
        crate::settings::set_setting(&conn, "playback_rate", "1.25").unwrap();

        let backup = export(&conn, "test").unwrap();
        assert!(!backup.contains("kept.mp3"), "local paths stay out of the backup");
        assert_eq!(inspect(&backup).unwrap(), (1, 3));

        // The library changes after the backup: a new podcast with a download.
        conn.execute("INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/other', 'Other', 1)", []).unwrap();
        let other = conn.last_insert_rowid();
        let stray = dir.path().join("stray.mp3");
        std::fs::write(&stray, b"audio").unwrap();
        add_episode(&conn, other, "stray", "downloaded", Some(&stray), true);
        crate::settings::set_setting(&conn, "playback_rate", "3").unwrap();

        let report = restore(&conn, &backup).unwrap();
        assert_eq!(report.podcasts, 1);
        assert_eq!(report.episodes, 3);
        assert_eq!(report.downloads_kept, 1);
        assert_eq!(report.files_to_delete, vec![stray]);

        let states: Vec<(String, String, Option<String>, i64)> = conn
            .prepare("SELECT guid, download_status, local_file_path, playback_position_ms FROM episodes ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            states,
            vec![
                ("kept".into(), "downloaded".into(), Some(kept.to_string_lossy().into()), 1234),
                ("queued-elsewhere".into(), "queued".into(), None, 1234),
                ("library-only".into(), "none".into(), None, 1234),
            ]
        );
        assert_eq!(crate::settings::get_setting(&conn, "playback_rate").as_deref(), Some("1.25"));
        let (rate, art): (Option<f64>, Option<String>) =
            conn.query_row("SELECT playback_rate, artwork_local_path FROM subscriptions", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((rate, art), (Some(1.5), None));
    }

    #[test]
    fn something_else_is_refused_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let conn = library(dir.path());
        assert!(matches!(restore(&conn, "{\"tables\": {}}"), Err(BackupError::NotABackup)));
        assert!(matches!(restore(&conn, "not json"), Err(BackupError::NotABackup)));
        let groups: i64 = conn.query_row("SELECT COUNT(*) FROM groups", [], |r| r.get(0)).unwrap();
        assert_eq!(groups, 1);
    }

    #[test]
    fn a_restore_with_broken_references_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let conn = library(dir.path());
        let backup = serde_json::json!({
            "format": FORMAT,
            "tables": { "groups": [{"id": 1, "name": "Default", "sort_order": 1.0}],
                        "subscriptions": [{"id": 5, "feed_url": "x", "title": "X", "group_id": 99}] }
        })
        .to_string();
        assert!(matches!(restore(&conn, &backup), Err(BackupError::ForeignKeys(_))));
        let groups: i64 = conn.query_row("SELECT COUNT(*) FROM groups", [], |r| r.get(0)).unwrap();
        assert_eq!(groups, 1, "the old library is still there");
    }
}
