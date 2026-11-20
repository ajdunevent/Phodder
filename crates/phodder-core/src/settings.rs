//! The app-wide `settings` key/value table.

use rusqlite::{params, Connection, OptionalExtension};

pub fn get_setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |row| row.get(0))
        .optional()
        .ok()
        .flatten()
}

pub fn get_setting_bool(conn: &Connection, key: &str) -> Option<bool> {
    get_setting(conn, key).map(|v| v == "true")
}

pub fn get_setting_i64(conn: &Connection, key: &str) -> Option<i64> {
    get_setting(conn, key).and_then(|v| v.parse().ok())
}

pub fn get_setting_f64(conn: &Connection, key: &str) -> Option<f64> {
    get_setting(conn, key).and_then(|v| v.parse().ok())
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// One device id per install for gpodder.net's per-device subscriptions
/// (Nextcloud has no device concept). Generated on first use from timestamp
/// and process id and persisted; it only needs to distinguish this install
/// from others on the same account.
pub fn get_or_create_gpodder_device_id(conn: &Connection) -> rusqlite::Result<String> {
    if let Some(existing) = get_setting(conn, "gpodder_device_id") {
        return Ok(existing);
    }
    let now = chrono::Utc::now();
    let device_id = format!("phodder-{}-{}", now.timestamp(), std::process::id());
    set_setting(conn, "gpodder_device_id", &device_id)?;
    Ok(device_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{migrate, open_connection};

    #[test]
    fn set_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        migrate(&conn).unwrap();

        assert_eq!(get_setting(&conn, "skip_forward_ms"), None);
        set_setting(&conn, "skip_forward_ms", "30000").unwrap();
        assert_eq!(get_setting_i64(&conn, "skip_forward_ms"), Some(30000));

        // ON CONFLICT overwrites.
        set_setting(&conn, "skip_forward_ms", "45000").unwrap();
        assert_eq!(get_setting_i64(&conn, "skip_forward_ms"), Some(45000));
    }

    #[test]
    fn get_setting_bool_only_true_string_is_true() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        migrate(&conn).unwrap();

        set_setting(&conn, "some_flag", "true").unwrap();
        assert_eq!(get_setting_bool(&conn, "some_flag"), Some(true));

        set_setting(&conn, "other_flag", "yes").unwrap();
        assert_eq!(get_setting_bool(&conn, "other_flag"), Some(false));
    }

    #[test]
    fn gpodder_device_id_is_generated_once_and_stays_stable() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        migrate(&conn).unwrap();

        assert_eq!(get_setting(&conn, "gpodder_device_id"), None);
        let first = get_or_create_gpodder_device_id(&conn).unwrap();
        assert!(!first.is_empty());
        let second = get_or_create_gpodder_device_id(&conn).unwrap();
        assert_eq!(first, second);
    }
}
