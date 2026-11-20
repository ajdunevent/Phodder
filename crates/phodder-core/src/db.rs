//! Database connections and schema versions. The window and the service
//! each open their own connections to the same SQLite file.
//!
//! The version lives in SQLite's `user_version`. New databases are created
//! from schema.sql; existing ones are brought up to date by MIGRATIONS.
//! Databases from before versioning (user_version 0, with tables) are made
//! to match version 1 exactly, keeping their rows.

use rusqlite::Connection;
use std::path::Path;

/// The version schema.sql describes. Bumped with each step added to
/// MIGRATIONS.
pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = include_str!("../schema.sql");
const SCHEMA_V1: &str = include_str!("../migrations/v1.sql");

/// `MIGRATIONS[i]` takes a database from version `i + 1` to `i + 2`. Each
/// runs in one transaction with its version bump, with foreign keys off so
/// a step can rebuild a table (SQLite's ALTER TABLE can't change most
/// constraints); they are checked before committing.
const MIGRATIONS: &[&str] = &[];

/// How long a process waits for another one migrating the same file.
const MIGRATION_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
/// open_connection's timeout, restored after migrating.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(150);

/// - `journal_mode = WAL`: readers don't block behind the writer.
/// - `synchronous = NORMAL`: cheaper than FULL; power loss can lose a few
///   seconds of position, not corrupt the file.
/// - `foreign_keys = ON`: SQLite turns enforcement off on every new connection.
pub fn open_connection(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

#[derive(Debug, PartialEq)]
pub enum MigrationOutcome {
    UpToDate,
    Created,
    /// The tables that had to be created or rebuilt to match version 1.
    Adopted { rebuilt: Vec<String> },
    Upgraded { from: i64 },
    /// Written by a newer Phodder. Left alone; most steps only add, so an
    /// older version usually still works with it.
    Newer(i64),
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("the database's foreign keys don't hold after migrating ({0} broken)")]
    ForeignKeys(usize),
    #[error("couldn't back up the database before migrating: {0}")]
    Backup(std::io::Error),
}

/// Creates or upgrades the schema. Before changing an existing database it
/// saves a copy beside it, `<file>.before-v<version>`.
pub fn migrate(conn: &Connection) -> Result<MigrationOutcome, MigrationError> {
    let version = user_version(conn)?;
    if version == SCHEMA_VERSION {
        return Ok(MigrationOutcome::UpToDate);
    }
    if version < SCHEMA_VERSION && has_tables(conn)? {
        back_up(conn)?;
    }

    conn.busy_timeout(MIGRATION_LOCK_WAIT)?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = migrate_locked(conn);
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    result
}

fn migrate_locked(conn: &Connection) -> Result<MigrationOutcome, MigrationError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        // Read again under the lock: another process may have just migrated.
        let mut version = user_version(conn)?;
        let mut outcome = MigrationOutcome::UpToDate;
        if version == 0 {
            if !has_tables(conn)? {
                conn.execute_batch(SCHEMA)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                return Ok(MigrationOutcome::Created);
            }
            outcome = MigrationOutcome::Adopted { rebuilt: adopt(conn)? };
            version = 1;
        }
        if version > SCHEMA_VERSION {
            return Ok(MigrationOutcome::Newer(version));
        }
        if version < SCHEMA_VERSION && outcome == MigrationOutcome::UpToDate {
            outcome = MigrationOutcome::Upgraded { from: version };
        }
        for step in &MIGRATIONS[(version - 1) as usize..] {
            conn.execute_batch(step)?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        let broken = conn.prepare("PRAGMA foreign_key_check")?.query_map([], |_| Ok(()))?.count();
        if broken > 0 {
            return Err(MigrationError::ForeignKeys(broken));
        }
        Ok(outcome)
    })();
    conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
    result
}

fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

fn has_tables(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table')", [], |row| row.get(0))
}

/// Written under a per-process name and renamed into place, so two
/// processes starting at once don't trip over each other's copy.
fn back_up(conn: &Connection) -> Result<(), MigrationError> {
    let Some(path) = conn.path().filter(|p| !p.is_empty()) else { return Ok(()) };
    let target = format!("{path}.before-v{SCHEMA_VERSION}");
    let partial = format!("{target}.{}", std::process::id());
    let _ = std::fs::remove_file(&partial);
    conn.execute("VACUUM INTO ?1", [&partial])?;
    std::fs::rename(&partial, &target).map_err(MigrationError::Backup)
}

/// Makes an unversioned database match version 1: missing tables are
/// created, tables whose columns or foreign keys differ are rebuilt with
/// their rows copied, and missing indexes are created.
fn adopt(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(SCHEMA_V1)?;
    let mut rebuilt = Vec::new();
    for (name, sql) in schema_objects(&reference, "table")? {
        if !object_exists(conn, "table", &name)? {
            conn.execute_batch(&sql)?;
            rebuilt.push(name);
            continue;
        }
        if table_shape(conn, &name)? == table_shape(&reference, &name)? {
            continue;
        }
        let wanted = column_names(&reference, &name)?;
        let present = column_names(conn, &name)?;
        let common: Vec<String> = wanted.into_iter().filter(|c| present.contains(c)).map(|c| format!("\"{c}\"")).collect();
        let temporary = format!("{name}_adopting");
        let create = sql.replacen(&format!("CREATE TABLE {name}"), &format!("CREATE TABLE \"{temporary}\""), 1);
        conn.execute_batch(&create)?;
        let columns = common.join(", ");
        conn.execute_batch(&format!(
            "INSERT INTO \"{temporary}\" ({columns}) SELECT {columns} FROM \"{name}\";
             DROP TABLE \"{name}\";
             ALTER TABLE \"{temporary}\" RENAME TO \"{name}\";"
        ))?;
        rebuilt.push(name);
    }
    // Rebuilding a table drops its indexes.
    for (name, sql) in schema_objects(&reference, "index")? {
        if !object_exists(conn, "index", &name)? {
            conn.execute_batch(&sql)?;
        }
    }
    Ok(rebuilt)
}

/// User-created objects of a type, with their CREATE statements. SQLite's
/// automatic indexes have no statement and are skipped.
fn schema_objects(conn: &Connection, kind: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut statement = conn.prepare("SELECT name, sql FROM sqlite_master WHERE type = ?1 AND sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
    let rows = statement.query_map([kind], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

fn object_exists(conn: &Connection, kind: &str, name: &str) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = ?1 AND name = ?2)", [kind, name], |row| row.get(0))
}

fn column_names(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let rows = statement.query_map([], |row| row.get(1))?;
    rows.collect()
}

/// Columns (name, type, not null, default, primary key) and foreign keys:
/// what makes two tables interchangeable.
fn table_shape(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut shape = Vec::new();
    let mut columns = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    for row in columns.query_map([], |row| {
        Ok(format!("column {} {} {} {:?} {}", row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?, row.get::<_, Option<String>>(4)?, row.get::<_, i64>(5)?))
    })? {
        shape.push(row?);
    }
    let mut keys = conn.prepare(&format!("PRAGMA foreign_key_list(\"{table}\")"))?;
    for row in keys.query_map([], |row| {
        Ok(format!("key {} {} {:?} {} {}", row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, Option<String>>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?))
    })? {
        shape.push(row?);
    }
    shape.sort();
    Ok(shape)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_of(conn: &Connection) -> Vec<String> {
        let mut shape = Vec::new();
        for (name, _) in schema_objects(conn, "table").unwrap() {
            shape.push(format!("table {name}"));
            shape.extend(table_shape(conn, &name).unwrap().into_iter().map(|line| format!("{name}: {line}")));
        }
        for (name, sql) in schema_objects(conn, "index").unwrap() {
            shape.push(format!("index {name}: {}", sql.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
        shape
    }

    #[test]
    fn fresh_database_applies_schema_and_seeds_default_group() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        assert_eq!(migrate(&conn).unwrap(), MigrationOutcome::Created);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);

        let group_name: String = conn
            .query_row("SELECT name FROM groups WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(group_name, "Default");
        assert_eq!(migrate(&conn).unwrap(), MigrationOutcome::UpToDate);
    }

    #[test]
    fn migrations_turn_version_1_into_the_current_schema() {
        // Fails when schema.sql changes without a matching step in
        // MIGRATIONS, which would leave existing databases behind.
        let migrated = Connection::open_in_memory().unwrap();
        migrated.execute_batch(SCHEMA_V1).unwrap();
        for step in MIGRATIONS {
            migrated.execute_batch(step).unwrap();
        }
        let fresh = Connection::open_in_memory().unwrap();
        fresh.execute_batch(SCHEMA).unwrap();
        assert_eq!(schema_of(&migrated), schema_of(&fresh));
        assert_eq!(MIGRATIONS.len() as i64, SCHEMA_VERSION - 1);
    }

    #[test]
    fn an_unversioned_database_is_brought_in_line_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        let conn = open_connection(&path).unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(
            "INSERT INTO subscriptions (id, feed_url, title, group_id) VALUES (5, 'https://example.com/feed', 'A Podcast', 1);
             INSERT INTO episodes (id, subscription_id, guid, title, audio_url, publish_date) VALUES (9, 5, 'g', 'An Episode', 'https://example.com/a.mp3', '2026-01-01T00:00:00Z');
             DROP INDEX idx_episodes_download_status;
             ALTER TABLE episodes DROP COLUMN skip_auto_enqueue;
             PRAGMA user_version = 0;",
        )
        .unwrap();
        let before = schema_of(&conn);

        let outcome = migrate(&conn).unwrap();
        assert_eq!(outcome, MigrationOutcome::Adopted { rebuilt: vec!["episodes".to_string()] });
        // Adopted to version 1, then upgraded like any other.
        let current = Connection::open_in_memory().unwrap();
        current.execute_batch(SCHEMA).unwrap();
        assert_ne!(before, schema_of(&current));
        assert_eq!(schema_of(&conn), schema_of(&current));
        let title: String = conn.query_row("SELECT title FROM episodes WHERE id = 9 AND subscription_id = 5", [], |row| row.get(0)).unwrap();
        assert_eq!(title, "An Episode");
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert!(dir.path().join(format!("old.db.before-v{SCHEMA_VERSION}")).exists());
        let foreign_keys: bool = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0)).unwrap();
        assert!(foreign_keys);
    }

    #[test]
    fn a_database_from_a_newer_version_is_left_alone() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 5).unwrap();
        assert_eq!(migrate(&conn).unwrap(), MigrationOutcome::Newer(SCHEMA_VERSION + 5));
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION + 5);
    }

    #[test]
    fn open_connection_sets_a_short_busy_timeout() {
        // rusqlite's 5000ms default froze the UI for up to five seconds
        // whenever a call on the GTK thread hit lock contention with the
        // backend.
        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        let timeout_ms: i32 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0)).unwrap();
        assert_eq!(timeout_ms, 150);
    }
}
