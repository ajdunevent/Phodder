//! `sync_accounts` and `sync_outbox` data access. `credential_ref` is a
//! lookup key into the Secret Service, resolved by the binary crate; this
//! crate never sees secrets.

use rusqlite::{Connection, OptionalExtension, params};

/// A configured sync account. At most one is enabled; the call sites enforce
/// that, not the schema.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncAccount {
    pub id: i64,
    /// "gpodder_net", "nextcloud_gpodder" or "goa_nextcloud" (schema.sql
    /// CHECK constraint). Nothing constructs "goa_nextcloud" yet; Nextcloud
    /// uses Login Flow v2.
    pub kind: String,
    pub server_url: Option<String>,
    pub login_name: String,
    pub credential_ref: Option<String>,
    pub sync_since_cursor_utc: Option<String>,
    pub last_sync_at_utc: Option<String>,
    /// Separate from sync_since_cursor_utc; see schema.sql.
    pub episode_actions_sync_cursor_utc: Option<String>,
    pub episode_actions_last_sync_at_utc: Option<String>,
    pub enabled: bool,
}

fn row_to_sync_account(row: &rusqlite::Row) -> rusqlite::Result<SyncAccount> {
    Ok(SyncAccount {
        id: row.get(0)?,
        kind: row.get(1)?,
        server_url: row.get(2)?,
        login_name: row.get(3)?,
        credential_ref: row.get(4)?,
        sync_since_cursor_utc: row.get(5)?,
        last_sync_at_utc: row.get(6)?,
        episode_actions_sync_cursor_utc: row.get(7)?,
        episode_actions_last_sync_at_utc: row.get(8)?,
        enabled: row.get(9)?,
    })
}

const SYNC_ACCOUNT_COLUMNS: &str =
    "id, kind, server_url, login_name, credential_ref, sync_since_cursor_utc, last_sync_at_utc, episode_actions_sync_cursor_utc, episode_actions_last_sync_at_utc, enabled";

/// `credential_ref` comes from the binary crate, which stored the secret.
pub fn insert_sync_account(
    conn: &Connection,
    kind: &str,
    server_url: Option<&str>,
    login_name: &str,
    credential_ref: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO sync_accounts (kind, server_url, login_name, credential_ref, enabled) VALUES (?1, ?2, ?3, ?4, 1)",
        params![kind, server_url, login_name, credential_ref],
    )?;
    Ok(conn.last_insert_rowid())
}

/// `None` when sync isn't set up.
pub fn get_enabled_sync_account(conn: &Connection) -> rusqlite::Result<Option<SyncAccount>> {
    conn.query_row(
        &format!("SELECT {SYNC_ACCOUNT_COLUMNS} FROM sync_accounts WHERE enabled = 1 ORDER BY id LIMIT 1"),
        [],
        row_to_sync_account,
    )
    .optional()
}

pub fn get_sync_account(conn: &Connection, account_id: i64) -> rusqlite::Result<Option<SyncAccount>> {
    conn.query_row(&format!("SELECT {SYNC_ACCOUNT_COLUMNS} FROM sync_accounts WHERE id = ?1"), params![account_id], row_to_sync_account)
        .optional()
}

/// Cursor and last-sync time are always written together, so a sync that
/// didn't complete can't advance the cursor. Both must already be UTC
/// (schema.sql convention).
pub fn update_sync_cursor(conn: &Connection, account_id: i64, cursor_utc: &str, synced_at_utc: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE sync_accounts SET sync_since_cursor_utc = ?2, last_sync_at_utc = ?3 WHERE id = ?1",
        params![account_id, cursor_utc, synced_at_utc],
    )?;
    Ok(())
}

/// As update_sync_cursor, for the episode-action cursor.
pub fn update_episode_actions_sync_cursor(conn: &Connection, account_id: i64, cursor_utc: &str, synced_at_utc: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE sync_accounts SET episode_actions_sync_cursor_utc = ?2, episode_actions_last_sync_at_utc = ?3 WHERE id = ?1",
        params![account_id, cursor_utc, synced_at_utc],
    )?;
    Ok(())
}

pub fn set_sync_account_enabled(conn: &Connection, account_id: i64, enabled: bool) -> rusqlite::Result<()> {
    conn.execute("UPDATE sync_accounts SET enabled = ?2 WHERE id = ?1", params![account_id, enabled])?;
    Ok(())
}

/// ON DELETE CASCADE removes the account's outbox rows. The binary crate must
/// revoke the credential server-side and remove it from the Secret Service
/// first, using `credential_ref`.
pub fn delete_sync_account(conn: &Connection, account_id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM sync_accounts WHERE id = ?1", params![account_id])?;
    Ok(())
}


/// A recorded episode action not yet pushed. The gpodder API is action-based
/// (a timestamped history), while `episodes` holds only current state, so
/// actions are logged here as they happen (see schema.sql).
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxAction {
    pub id: i64,
    pub sync_account_id: i64,
    pub episode_id: i64,
    /// "play", "download", "delete" or "new" (schema.sql CHECK constraint).
    pub action_type: String,
    /// "play" only.
    pub position_ms: Option<i64>,
    pub action_at_utc: String,
    pub pushed: bool,
}

/// Recorded when the local state changes, not derived from `episodes` later.
pub fn enqueue_outbox_action(
    conn: &Connection,
    sync_account_id: i64,
    episode_id: i64,
    action_type: &str,
    position_ms: Option<i64>,
    action_at_utc: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_outbox (sync_account_id, episode_id, action_type, position_ms, action_at_utc, pushed) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        params![sync_account_id, episode_id, action_type, position_ms, action_at_utc],
    )?;
    Ok(())
}

/// Oldest first, the order they happened.
pub fn pending_outbox_actions(conn: &Connection, sync_account_id: i64) -> rusqlite::Result<Vec<OutboxAction>> {
    let mut stmt = conn.prepare(
        "SELECT id, sync_account_id, episode_id, action_type, position_ms, action_at_utc, pushed
         FROM sync_outbox WHERE sync_account_id = ?1 AND pushed = 0 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![sync_account_id], |row| {
        Ok(OutboxAction {
            id: row.get(0)?,
            sync_account_id: row.get(1)?,
            episode_id: row.get(2)?,
            action_type: row.get(3)?,
            position_ms: row.get(4)?,
            action_at_utc: row.get(5)?,
            pushed: row.get(6)?,
        })
    })?;
    rows.collect()
}

/// Called after the server accepted the upload.
pub fn mark_outbox_actions_pushed(conn: &Connection, action_ids: &[i64]) -> rusqlite::Result<()> {
    for id in action_ids {
        conn.execute("UPDATE sync_outbox SET pushed = 1 WHERE id = ?1", params![id])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn insert_and_fetch_the_enabled_account() {
        let conn = setup();
        assert_eq!(get_enabled_sync_account(&conn).unwrap(), None);

        let id = insert_sync_account(&conn, "nextcloud_gpodder", Some("https://cloud.example.com"), "alice", Some("ref-123")).unwrap();
        let account = get_enabled_sync_account(&conn).unwrap().unwrap();
        assert_eq!(account.id, id);
        assert_eq!(account.kind, "nextcloud_gpodder");
        assert_eq!(account.server_url, Some("https://cloud.example.com".to_string()));
        assert_eq!(account.login_name, "alice");
        assert_eq!(account.credential_ref, Some("ref-123".to_string()));
        assert!(account.enabled);
        assert_eq!(account.sync_since_cursor_utc, None);
        assert_eq!(account.episode_actions_sync_cursor_utc, None);
        assert_eq!(account.episode_actions_last_sync_at_utc, None);
    }

    #[test]
    fn update_episode_actions_sync_cursor_is_independent_of_subscription_cursor() {
        let conn = setup();
        let id = insert_sync_account(&conn, "gpodder_net", None, "carol", None).unwrap();
        update_sync_cursor(&conn, id, "1663540502", "2026-01-01T00:00:00Z").unwrap();
        update_episode_actions_sync_cursor(&conn, id, "1663540600", "2026-01-01T00:05:00Z").unwrap();

        let account = get_sync_account(&conn, id).unwrap().unwrap();
        // Updating one cursor leaves the other alone.
        assert_eq!(account.sync_since_cursor_utc, Some("1663540502".to_string()));
        assert_eq!(account.last_sync_at_utc, Some("2026-01-01T00:00:00Z".to_string()));
        assert_eq!(account.episode_actions_sync_cursor_utc, Some("1663540600".to_string()));
        assert_eq!(account.episode_actions_last_sync_at_utc, Some("2026-01-01T00:05:00Z".to_string()));
    }

    #[test]
    fn disabling_an_account_removes_it_from_the_enabled_lookup() {
        let conn = setup();
        let id = insert_sync_account(&conn, "gpodder_net", None, "bob", Some("ref-456")).unwrap();
        set_sync_account_enabled(&conn, id, false).unwrap();
        assert_eq!(get_enabled_sync_account(&conn).unwrap(), None);
        // Still fetchable by id, not through the enabled lookup.
        assert!(get_sync_account(&conn, id).unwrap().is_some());
    }

    #[test]
    fn update_sync_cursor_sets_both_fields_together() {
        let conn = setup();
        let id = insert_sync_account(&conn, "gpodder_net", None, "carol", None).unwrap();
        update_sync_cursor(&conn, id, "1663540502", "2026-01-01T00:00:00Z").unwrap();
        let account = get_sync_account(&conn, id).unwrap().unwrap();
        assert_eq!(account.sync_since_cursor_utc, Some("1663540502".to_string()));
        assert_eq!(account.last_sync_at_utc, Some("2026-01-01T00:00:00Z".to_string()));
    }

    #[test]
    fn deleting_an_account_cascades_to_its_own_outbox_rows() {
        let conn = setup();
        let account_id = insert_sync_account(&conn, "gpodder_net", None, "dave", None).unwrap();
        let episode_id = insert_test_episode(&conn);
        enqueue_outbox_action(&conn, account_id, episode_id, "download", None, "2026-01-01T00:00:00Z").unwrap();
        assert_eq!(pending_outbox_actions(&conn, account_id).unwrap().len(), 1);

        delete_sync_account(&conn, account_id).unwrap();

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM sync_outbox", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "ON DELETE CASCADE should have removed the outbox row along with its account");
    }

    #[test]
    fn pending_actions_excludes_already_pushed_ones() {
        let conn = setup();
        let account_id = insert_sync_account(&conn, "gpodder_net", None, "erin", None).unwrap();
        let episode_id = insert_test_episode(&conn);
        enqueue_outbox_action(&conn, account_id, episode_id, "play", Some(120_000), "2026-01-01T00:00:00Z").unwrap();
        enqueue_outbox_action(&conn, account_id, episode_id, "download", None, "2026-01-01T00:01:00Z").unwrap();

        let pending = pending_outbox_actions(&conn, account_id).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].action_type, "play");
        assert_eq!(pending[0].position_ms, Some(120_000));

        mark_outbox_actions_pushed(&conn, &[pending[0].id]).unwrap();
        let still_pending = pending_outbox_actions(&conn, account_id).unwrap();
        assert_eq!(still_pending.len(), 1);
        assert_eq!(still_pending[0].action_type, "download");
    }

    #[test]
    fn mark_pushed_with_empty_ids_is_a_harmless_no_op() {
        let conn = setup();
        mark_outbox_actions_pushed(&conn, &[]).unwrap();
    }

    /// sync_outbox's episode_id needs a real episode row.
    fn insert_test_episode(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO groups (name, sort_order) VALUES ('Test Group', 0)",
            [],
        )
        .unwrap();
        let group_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/feed.xml', 'Test Show', ?1)",
            params![group_id],
        )
        .unwrap();
        let subscription_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date) VALUES (?1, 'guid-1', 'Test Episode', 'https://example.com/ep.mp3', '2026-01-01T00:00:00Z')",
            params![subscription_id],
        )
        .unwrap();
        conn.last_insert_rowid()
    }
}
