//! Subscription and episode queries for refresh, the UI and sync.

use crate::feed::ParsedEpisode;
use crate::models::Subscription;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;

pub fn list_subscriptions(conn: &Connection) -> rusqlite::Result<Vec<Subscription>> {
    let mut stmt = conn.prepare(
        "SELECT id, feed_url, title, artwork_url, group_id, auto_download_enabled,
                notify_on_new_episodes, download_filter_rules, chapter_skip_rules,
                delete_after_minutes, description, artwork_local_path, feed_title
         FROM subscriptions
         WHERE active = 1",
    )?;
    let rows = stmt.query_map([], row_to_subscription)?;
    rows.collect()
}

fn row_to_subscription(row: &rusqlite::Row) -> rusqlite::Result<Subscription> {
    let filter_rules_json: String = row.get(7)?;
    let chapter_rules_json: String = row.get(8)?;
    Ok(Subscription {
        id: row.get(0)?,
        feed_url: row.get(1)?,
        title: row.get(2)?,
        artwork_url: row.get(3)?,
        group_id: row.get(4)?,
        auto_download_enabled: row.get::<_, i64>(5)? != 0,
        notify_on_new_episodes: row.get::<_, i64>(6)? != 0,
        // Only this app writes these columns, but a corrupt rule set falls
        // back to no rules so it can't break loading every subscription.
        download_filter_rules: serde_json::from_str(&filter_rules_json).unwrap_or_else(|e| {
            tracing::warn!("failed to parse download_filter_rules JSON: {e}");
            Vec::new()
        }),
        chapter_skip_rules: serde_json::from_str(&chapter_rules_json).unwrap_or_else(|e| {
            tracing::warn!("failed to parse chapter_skip_rules JSON: {e}");
            Vec::new()
        }),
        delete_after_minutes: row.get(9)?,
        description: row.get(10)?,
        artwork_local_path: row.get(11)?,
        feed_title: row.get(12)?,
    })
}

/// GUIDs already stored for a subscription, to dedupe a parsed feed before
/// inserting.
pub fn existing_episode_guids(conn: &Connection, subscription_id: i64) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT guid FROM episodes WHERE subscription_id = ?1")?;
    let rows = stmt.query_map(params![subscription_id], |row| row.get(0))?;
    rows.collect()
}

/// Inserts a newly seen episode as 'queued' when `queued_for_download`
/// (auto-download on and the filters allowed it), otherwise 'none'.
pub fn insert_episode(
    conn: &Connection,
    subscription_id: i64,
    parsed: &ParsedEpisode,
    queued_for_download: bool,
    artwork_local_path: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO episodes
            (subscription_id, guid, title, show_notes_html, show_notes_text,
             audio_url, duration_ms, publish_date, chapters_url, transcript_url,
             download_status, artwork_url, artwork_local_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            subscription_id,
            parsed.guid,
            parsed.title,
            parsed.show_notes_html,
            parsed.show_notes_html.as_deref().map(strip_html_tags),
            parsed.audio_url,
            parsed.duration_ms,
            parsed.publish_date.to_rfc3339(),
            parsed.chapters_url,
            parsed.transcript_url,
            if queued_for_download { "queued" } else { "none" },
            parsed.artwork_url,
            artwork_local_path,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Conditional-GET bookkeeping and last-refresh time, recorded even when
/// nothing was new.
pub fn record_refresh(
    conn: &Connection,
    subscription_id: i64,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE subscriptions
         SET etag = ?1, last_modified = ?2, last_refreshed_at = ?3
         WHERE id = ?4",
        params![etag, last_modified, chrono::Utc::now().to_rfc3339(), subscription_id],
    )?;
    Ok(())
}

/// Crude tag stripping for `show_notes_text`, which only feeds filter
/// matching; display renders the HTML with html2pango.
fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result
}

#[derive(Debug, Clone)]
pub struct EpisodeSummary {
    pub id: i64,
    pub title: String,
    pub podcast_title: String,
    pub download_status: String,
    pub file_size_bytes: Option<i64>,
    pub duration_ms: Option<i64>,
    /// RFC 3339; the UI formats it.
    pub publish_date: String,
    /// `content:encoded` preferred; the UI renders it.
    pub show_notes_html: Option<String>,
    /// Episode artwork, usually `None`; fall back to
    /// `subscription_artwork_local_path`.
    pub artwork_local_path: Option<String>,
    /// The fallback thumbnail.
    pub subscription_artwork_local_path: Option<String>,
    /// Colors Library's download-progress fill (see schema.sql
    /// `groups.color`).
    pub group_color: Option<String>,
    /// In the playback queue (`group_queue_position IS NOT NULL`); hides the
    /// add-to-queue button.
    pub is_queued: bool,
}

/// How a back-catalog episode shows in a subscription's Episodes list.
/// 'queued', 'downloading' and 'failed' count as NeverDownloaded: there, "not
/// yet playable" is the distinction that matters, and Library shows the exact
/// state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackCatalogStatus {
    NeverDownloaded,
    DownloadedUnplayed,
    Played,
}

/// Status plus the database's probed duration, which beats the feed's value;
/// reading the feed's duration showed wrong lengths for downloaded episodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackCatalogEntry {
    pub status: BackCatalogStatus,
    pub duration_ms: Option<i64>,
    /// The database id, needed to enqueue an already-downloaded episode.
    pub episode_id: i64,
    /// As EpisodeSummary::is_queued.
    pub is_queued: bool,
    /// The file is on disk. Separate from `status`, where Played wins: a
    /// played episode is often already deleted.
    pub downloaded: bool,
    /// The last download attempt failed.
    pub failed: bool,
    /// Waiting to download or downloading.
    pub pending: bool,
}

/// Known episodes keyed by guid. A guid missing from the map (most of a long
/// show's catalog) means NeverDownloaded with no known duration.
pub fn back_catalog_statuses(conn: &Connection, subscription_id: i64) -> rusqlite::Result<std::collections::HashMap<String, BackCatalogEntry>> {
    let mut stmt =
        conn.prepare("SELECT guid, played, download_status, duration_ms, id, group_queue_position IS NOT NULL FROM episodes WHERE subscription_id = ?1")?;
    let rows = stmt.query_map(params![subscription_id], |row| {
        let guid: String = row.get(0)?;
        let played: bool = row.get(1)?;
        let download_status: String = row.get(2)?;
        let duration_ms: Option<i64> = row.get(3)?;
        let episode_id: i64 = row.get(4)?;
        let is_queued: bool = row.get(5)?;
        let downloaded = download_status == "downloaded";
        let failed = download_status == "failed";
        let pending = download_status == "queued" || download_status == "downloading";
        let status = if played {
            BackCatalogStatus::Played
        } else if downloaded {
            BackCatalogStatus::DownloadedUnplayed
        } else {
            BackCatalogStatus::NeverDownloaded
        };
        Ok((guid, BackCatalogEntry { status, duration_ms, episode_id, is_queued, downloaded, failed, pending }))
    })?;
    rows.collect()
}

/// The chapter-skip rules of an episode's podcast; empty when unparseable.
pub fn chapter_skip_rules_for_episode(conn: &Connection, episode_id: i64) -> rusqlite::Result<Vec<crate::filter::FilterRule>> {
    let json: String = conn.query_row(
        "SELECT s.chapter_skip_rules FROM episodes e JOIN subscriptions s ON s.id = e.subscription_id WHERE e.id = ?1",
        params![episode_id],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&json).unwrap_or_default())
}

/// Group name and episode count per subscription, for the Subscriptions tab
/// subtitle, fetched in one query. Counts episodes with `download_status !=
/// 'none'`, as Library does.
#[derive(Debug, Clone)]
pub struct SubscriptionGroupInfo {
    pub group_name: String,
    pub episode_count: i64,
}

pub fn subscription_group_names_and_episode_counts(
    conn: &Connection,
) -> rusqlite::Result<std::collections::HashMap<i64, SubscriptionGroupInfo>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, g.name,
                (SELECT COUNT(*) FROM episodes e WHERE e.subscription_id = s.id AND e.download_status != 'none')
         FROM subscriptions s
         JOIN groups g ON g.id = s.group_id
         WHERE s.active = 1",
    )?;
    let rows = stmt.query_map([], |row| {
        let subscription_id: i64 = row.get(0)?;
        let group_name: String = row.get(1)?;
        let episode_count: i64 = row.get(2)?;
        Ok((subscription_id, SubscriptionGroupInfo { group_name, episode_count }))
    })?;
    rows.collect()
}

/// Library: episodes with any status besides 'none' (queued, downloading,
/// downloaded or failed), newest published first.
pub fn list_episodes_with_status(conn: &Connection) -> rusqlite::Result<Vec<EpisodeSummary>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.title, s.title, e.download_status, e.file_size_bytes,
                e.show_notes_html, e.artwork_local_path, s.artwork_local_path, e.duration_ms,
                e.publish_date, g.color, e.group_queue_position IS NOT NULL
         FROM episodes e
         JOIN subscriptions s ON s.id = e.subscription_id
         JOIN groups g ON g.id = s.group_id
         WHERE e.download_status != 'none'
         ORDER BY e.publish_date DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(EpisodeSummary {
            id: row.get(0)?,
            title: row.get(1)?,
            podcast_title: row.get(2)?,
            download_status: row.get(3)?,
            file_size_bytes: row.get(4)?,
            show_notes_html: row.get(5)?,
            artwork_local_path: row.get(6)?,
            subscription_artwork_local_path: row.get(7)?,
            duration_ms: row.get(8)?,
            publish_date: row.get(9)?,
            group_color: row.get(10)?,
            is_queued: row.get(11)?,
        })
    })?;
    rows.collect()
}

/// History: EpisodeSummary rows with `played_locally`, newest `played_order`
/// first. `played` alone isn't enough: sync sets it for episodes finished on
/// other devices (see schema.sql).
pub fn list_history_episodes(conn: &Connection) -> rusqlite::Result<Vec<EpisodeSummary>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.title, s.title, e.download_status, e.file_size_bytes,
                e.show_notes_html, e.artwork_local_path, s.artwork_local_path, e.duration_ms,
                e.publish_date, g.color, e.group_queue_position IS NOT NULL
         FROM episodes e
         JOIN subscriptions s ON s.id = e.subscription_id
         JOIN groups g ON g.id = s.group_id
         WHERE e.played_locally = 1
         ORDER BY e.played_order DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(EpisodeSummary {
            id: row.get(0)?,
            title: row.get(1)?,
            podcast_title: row.get(2)?,
            download_status: row.get(3)?,
            file_size_bytes: row.get(4)?,
            show_notes_html: row.get(5)?,
            artwork_local_path: row.get(6)?,
            subscription_artwork_local_path: row.get(7)?,
            duration_ms: row.get(8)?,
            publish_date: row.get(9)?,
            group_color: row.get(10)?,
            is_queued: row.get(11)?,
        })
    })?;
    rows.collect()
}

/// History's Clear removes the row outright; the episode reappears as never
/// played if the feed is browsed again. Callers delete its file first.
pub fn clear_from_history(conn: &Connection, episode_id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM episodes WHERE id = ?1", params![episode_id])?;
    Ok(())
}

/// Inserts a subscription, or reactivates a tombstoned row for the same
/// feed_url (unsubscribed, removal not yet synced), which UNIQUE(feed_url)
/// would otherwise reject. `title` is a placeholder (often the URL) until the
/// first refresh. Reactivation clears `synced_with_account_id` so the next
/// sync reports it as added.
pub fn insert_subscription(conn: &Connection, feed_url: &str, initial_title: &str, group_id: i64) -> rusqlite::Result<i64> {
    let existing_tombstone: Option<i64> = conn
        .query_row("SELECT id FROM subscriptions WHERE feed_url = ?1 AND active = 0", params![feed_url], |row| row.get(0))
        .optional()?;
    if let Some(existing_id) = existing_tombstone {
        conn.execute(
            "UPDATE subscriptions SET active = 1, synced_with_account_id = NULL, title = ?2, group_id = ?3 WHERE id = ?1",
            params![existing_id, initial_title, group_id],
        )?;
        return Ok(existing_id);
    }
    conn.execute(
        "INSERT INTO subscriptions (feed_url, title, group_id) VALUES (?1, ?2, ?3)",
        params![feed_url, initial_title, group_id],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_subscription(conn: &Connection, id: i64) -> rusqlite::Result<Option<Subscription>> {
    conn.query_row(
        "SELECT id, feed_url, title, artwork_url, group_id, auto_download_enabled,
                notify_on_new_episodes, download_filter_rules, chapter_skip_rules,
                delete_after_minutes, description, artwork_local_path, feed_title
         FROM subscriptions WHERE id = ?1",
        params![id],
        row_to_subscription,
    )
    .optional()
}

/// Add Feed uses this to open the existing subscription instead of failing on
/// the UNIQUE constraint.
pub fn get_subscription_by_feed_url(conn: &Connection, feed_url: &str) -> rusqlite::Result<Option<Subscription>> {
    conn.query_row(
        "SELECT id, feed_url, title, artwork_url, group_id, auto_download_enabled,
                notify_on_new_episodes, download_filter_rules, chapter_skip_rules,
                delete_after_minutes, description, artwork_local_path, feed_title
         FROM subscriptions WHERE feed_url = ?1",
        params![feed_url],
        row_to_subscription,
    )
    .optional()
}

#[derive(Debug, Clone)]
pub struct EpisodeDetail {
    pub title: String,
    pub podcast_title: String,
    /// `content:encoded` preferred; the UI renders it.
    pub show_notes_html: Option<String>,
    pub duration_ms: Option<i64>,
    pub download_status: String,
    pub file_size_bytes: Option<i64>,
    pub publish_date: String,
    pub played: bool,
    pub playback_position_ms: i64,
    pub queued: bool,
    /// Episode artwork, falling back to the show's.
    pub artwork_local_path: Option<String>,
    pub local_file_path: Option<String>,
}

/// Everything the episode details view shows, including show notes, which
/// `episode_display_info` leaves out.
pub fn get_episode_detail(conn: &Connection, episode_id: i64) -> rusqlite::Result<Option<EpisodeDetail>> {
    conn.query_row(
        "SELECT e.title, s.title, e.show_notes_html, e.duration_ms,
                e.download_status, e.file_size_bytes, e.publish_date, e.played,
                e.playback_position_ms, e.group_queue_position IS NOT NULL,
                COALESCE(e.artwork_local_path, s.artwork_local_path), e.local_file_path
         FROM episodes e
         JOIN subscriptions s ON s.id = e.subscription_id
         WHERE e.id = ?1",
        params![episode_id],
        |row| {
            Ok(EpisodeDetail {
                title: row.get(0)?,
                podcast_title: row.get(1)?,
                show_notes_html: row.get(2)?,
                duration_ms: row.get(3)?,
                download_status: row.get(4)?,
                file_size_bytes: row.get(5)?,
                publish_date: row.get(6)?,
                played: row.get(7)?,
                playback_position_ms: row.get(8)?,
                queued: row.get(9)?,
                artwork_local_path: row.get(10)?,
                local_file_path: row.get(11)?,
            })
        },
    )
    .optional()
}

/// Called every refresh so renames and new artwork follow the feed. Leaves
/// `title` alone when `title_manually_set`; `feed_title` always tracks the
/// feed.
pub fn update_subscription_metadata(
    conn: &Connection,
    subscription_id: i64,
    title: &str,
    artwork_url: Option<&str>,
    description: Option<&str>,
    artwork_local_path: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE subscriptions
         SET title = CASE WHEN title_manually_set = 0 THEN ?1 ELSE title END,
             feed_title = ?1,
             artwork_url = ?2, description = ?3, artwork_local_path = ?4
         WHERE id = ?5",
        params![title, artwork_url, description, artwork_local_path, subscription_id],
    )?;
    Ok(())
}

/// Downloaded files for a subscription; the caller deletes them.
pub fn episode_file_paths_for_subscription(conn: &Connection, subscription_id: i64) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT local_file_path FROM episodes WHERE subscription_id = ?1 AND local_file_path IS NOT NULL",
    )?;
    let rows = stmt.query_map(params![subscription_id], |row| row.get::<_, String>(0))?;
    rows.collect()
}

/// Every episode id, including ones mid-download, so the caller can cancel
/// downloads before deleting.
pub fn episode_ids_for_subscription(conn: &Connection, subscription_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM episodes WHERE subscription_id = ?1")?;
    let rows = stmt.query_map(params![subscription_id], |row| row.get(0))?;
    rows.collect()
}

/// Episodes go with it through ON DELETE CASCADE (foreign_keys is on in
/// open_connection); their files don't. Delete those first via
/// episode_file_paths_for_subscription.
pub fn delete_subscription(conn: &Connection, subscription_id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM subscriptions WHERE id = ?1", params![subscription_id])?;
    Ok(())
}

/// Marks a subscription inactive so its removal can be reported to a sync
/// server later (see schema.sql). Episodes are deleted at once; only the
/// subscription row stays, so joins need no `active` filter. Clears
/// `synced_with_account_id`: this removal hasn't been reported anywhere.
pub fn tombstone_subscription(conn: &Connection, subscription_id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM episodes WHERE subscription_id = ?1", params![subscription_id])?;
    conn.execute("UPDATE subscriptions SET active = 0, synced_with_account_id = NULL WHERE id = ?1", params![subscription_id])?;
    Ok(())
}

/// Moves a subscription to another group. The UI's picker comes from
/// `groups::list_groups`, so the id isn't validated here.
pub fn update_subscription_group(conn: &Connection, subscription_id: i64, group_id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE subscriptions SET group_id = ?1 WHERE id = ?2", params![group_id, subscription_id])?;
    Ok(())
}

/// Sets a custom title and `title_manually_set`, so refreshes stop
/// overwriting it. Setting it back to the feed's current title clears the
/// flag.
pub fn update_subscription_title(conn: &Connection, subscription_id: i64, title: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE subscriptions SET title = ?1, title_manually_set = CASE WHEN ?1 IS feed_title THEN 0 ELSE 1 END WHERE id = ?2",
        params![title, subscription_id],
    )?;
    Ok(())
}

/// Auto-download, notifications, filter rules and expiry (minutes)
/// together. Filter rules are stored as JSON; a serialization error is
/// wrapped in rusqlite's error type, since this crate doesn't use anyhow.
pub fn update_subscription_settings(
    conn: &Connection,
    subscription_id: i64,
    auto_download_enabled: bool,
    notify_on_new_episodes: bool,
    download_filter_rules: &[crate::filter::FilterRule],
    delete_after_minutes: i64,
) -> rusqlite::Result<()> {
    let rules_json = serde_json::to_string(download_filter_rules)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "UPDATE subscriptions
         SET auto_download_enabled = ?1, notify_on_new_episodes = ?2, download_filter_rules = ?3,
             delete_after_minutes = ?4
         WHERE id = ?5",
        params![auto_download_enabled, notify_on_new_episodes, rules_json, delete_after_minutes, subscription_id],
    )?;
    Ok(())
}

/// Chapter titles to skip during playback; only `pattern` is used.
pub fn set_chapter_skip_rules(conn: &Connection, subscription_id: i64, rules: &[crate::filter::FilterRule]) -> rusqlite::Result<()> {
    let json = serde_json::to_string(rules).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute("UPDATE subscriptions SET chapter_skip_rules = ?1 WHERE id = ?2", params![json, subscription_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{migrate, open_connection};
    use chrono::Utc;

    fn setup() -> (tempfile::TempDir, Connection, i64) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/feed', 'Test Show', 1)",
            [],
        )
        .unwrap();
        let subscription_id = conn.last_insert_rowid();
        (dir, conn, subscription_id)
    }

    #[test]
    fn update_subscription_group_reassigns_group_id() {
        let (_dir, conn, subscription_id) = setup();
        conn.execute("INSERT INTO groups (id, name, sort_order) VALUES (2, 'Later', 2.0)", []).unwrap();

        update_subscription_group(&conn, subscription_id, 2).unwrap();

        let loaded = get_subscription(&conn, subscription_id).unwrap().unwrap();
        assert_eq!(loaded.group_id, 2);
    }

    fn sample_parsed_episode(guid: &str) -> ParsedEpisode {
        ParsedEpisode {
            guid: guid.to_string(),
            title: "Test Episode".to_string(),
            show_notes_html: Some("<p>Hello <b>world</b></p>".to_string()),
            audio_url: "https://example.com/ep.mp3".to_string(),
            file_size_bytes: Some(1_000_000),
            duration_ms: Some(60_000),
            publish_date: Utc::now(),
            chapters_url: None,
            transcript_url: None,
            artwork_url: None,
        }
    }

    #[test]
    fn insert_then_list_round_trips_subscription_fields() {
        let (_dir, conn, sub_id) = setup();
        let subs = list_subscriptions(&conn).unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, sub_id);
        assert_eq!(subs[0].title, "Test Show");
        assert!(subs[0].download_filter_rules.is_empty());
    }

    #[test]
    fn existing_episode_guids_reflects_inserts() {
        let (_dir, conn, sub_id) = setup();
        assert!(existing_episode_guids(&conn, sub_id).unwrap().is_empty());

        insert_episode(&conn, sub_id, &sample_parsed_episode("ep-1"), false, None).unwrap();
        let guids = existing_episode_guids(&conn, sub_id).unwrap();
        assert_eq!(guids.len(), 1);
        assert!(guids.contains("ep-1"));
    }

    #[test]
    fn insert_episode_strips_html_for_show_notes_text_but_keeps_html_version() {
        let (_dir, conn, sub_id) = setup();
        insert_episode(&conn, sub_id, &sample_parsed_episode("ep-1"), false, None).unwrap();

        let (html, text): (String, String) = conn
            .query_row(
                "SELECT show_notes_html, show_notes_text FROM episodes WHERE guid = 'ep-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(html, "<p>Hello <b>world</b></p>");
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn queued_for_download_flag_sets_download_status() {
        let (_dir, conn, sub_id) = setup();
        insert_episode(&conn, sub_id, &sample_parsed_episode("ep-1"), true, None).unwrap();
        insert_episode(&conn, sub_id, &sample_parsed_episode("ep-2"), false, None).unwrap();

        let status1: String = conn
            .query_row("SELECT download_status FROM episodes WHERE guid = 'ep-1'", [], |r| r.get(0))
            .unwrap();
        let status2: String = conn
            .query_row("SELECT download_status FROM episodes WHERE guid = 'ep-2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status1, "queued");
        assert_eq!(status2, "none");
    }

    #[test]
    fn record_refresh_updates_conditional_get_bookkeeping() {
        let (_dir, conn, sub_id) = setup();
        record_refresh(&conn, sub_id, Some("\"abc123\""), Some("Wed, 02 Oct 2024 15:00:00 GMT")).unwrap();

        let (etag, last_modified, refreshed_at): (Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT etag, last_modified, last_refreshed_at FROM subscriptions WHERE id = ?1",
                params![sub_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(etag.as_deref(), Some("\"abc123\""));
        assert_eq!(last_modified.as_deref(), Some("Wed, 02 Oct 2024 15:00:00 GMT"));
        assert!(refreshed_at.is_some());
    }

    #[test]
    fn list_episodes_with_status_excludes_none_and_joins_podcast_title() {
        let (_dir, conn, sub_id) = setup();
        insert_episode(&conn, sub_id, &sample_parsed_episode("downloaded-ep"), false, None).unwrap();
        conn.execute(
            "UPDATE episodes SET download_status = 'downloaded' WHERE guid = 'downloaded-ep'",
            [],
        )
        .unwrap();
        insert_episode(&conn, sub_id, &sample_parsed_episode("never-queued-ep"), false, None).unwrap();
        // 'none' episodes aren't listed.

        let episodes = list_episodes_with_status(&conn).unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].title, "Test Episode");
        assert_eq!(episodes[0].podcast_title, "Test Show");
        assert_eq!(episodes[0].download_status, "downloaded");
        assert_eq!(episodes[0].show_notes_html.as_deref(), Some("<p>Hello <b>world</b></p>"));
    }

    #[test]
    fn list_episodes_with_status_reflects_playback_queue_membership() {
        // Library hides its add button for queued episodes.
        let (_dir, conn, sub_id) = setup();
        let episode_id = insert_episode(&conn, sub_id, &sample_parsed_episode("downloaded-and-queued"), false, None).unwrap();
        conn.execute("UPDATE episodes SET download_status = 'downloaded', group_queue_position = 1.0 WHERE id = ?1", params![episode_id]).unwrap();

        let episode_id_2 = insert_episode(&conn, sub_id, &sample_parsed_episode("downloaded-not-queued"), false, None).unwrap();
        conn.execute("UPDATE episodes SET download_status = 'downloaded' WHERE id = ?1", params![episode_id_2]).unwrap();

        let episodes = list_episodes_with_status(&conn).unwrap();
        let queued = episodes.iter().find(|e| e.id == episode_id).unwrap();
        let not_queued = episodes.iter().find(|e| e.id == episode_id_2).unwrap();
        assert!(queued.is_queued);
        assert!(!not_queued.is_queued);
    }

    #[test]
    fn list_episodes_with_status_falls_back_to_subscription_artwork() {
        let (_dir, conn, sub_id) = setup();
        conn.execute(
            "UPDATE subscriptions SET artwork_local_path = '/tmp/phodder-downloads/artwork-sub.img' WHERE id = ?1",
            params![sub_id],
        )
        .unwrap();

        // Falls back to the show's artwork.
        insert_episode(&conn, sub_id, &sample_parsed_episode("plain-ep"), false, None).unwrap();
        conn.execute("UPDATE episodes SET download_status = 'downloaded' WHERE guid = 'plain-ep'", [])
            .unwrap();

        // Episode artwork takes priority.
        insert_episode(
            &conn,
            sub_id,
            &sample_parsed_episode("own-art-ep"),
            false,
            Some("/tmp/phodder-downloads/artwork-ep.img"),
        )
        .unwrap();
        conn.execute("UPDATE episodes SET download_status = 'downloaded' WHERE guid = 'own-art-ep'", [])
            .unwrap();

        let episodes = list_episodes_with_status(&conn).unwrap();
        let plain = episodes.iter().find(|e| e.title == "Test Episode" && e.artwork_local_path.is_none()).unwrap();
        assert_eq!(plain.subscription_artwork_local_path.as_deref(), Some("/tmp/phodder-downloads/artwork-sub.img"));

        let own_art = episodes.iter().find(|e| e.artwork_local_path.is_some()).unwrap();
        assert_eq!(own_art.artwork_local_path.as_deref(), Some("/tmp/phodder-downloads/artwork-ep.img"));
        assert_eq!(own_art.subscription_artwork_local_path.as_deref(), Some("/tmp/phodder-downloads/artwork-sub.img"));
    }

    #[test]
    fn back_catalog_statuses_covers_all_three_states() {
        let (_dir, conn, sub_id) = setup();
        insert_episode(&conn, sub_id, &sample_parsed_episode("downloaded-unplayed"), false, None).unwrap();
        conn.execute(
            "UPDATE episodes SET download_status = 'downloaded' WHERE guid = 'downloaded-unplayed'",
            [],
        )
        .unwrap();

        insert_episode(&conn, sub_id, &sample_parsed_episode("played-ep"), false, None).unwrap();
        conn.execute("UPDATE episodes SET download_status = 'downloaded', played = 1 WHERE guid = 'played-ep'", [])
            .unwrap();

        insert_episode(&conn, sub_id, &sample_parsed_episode("queued-ep"), true, None).unwrap();

        let statuses = back_catalog_statuses(&conn, sub_id).unwrap();
        assert_eq!(statuses.get("downloaded-unplayed").map(|e| e.status), Some(BackCatalogStatus::DownloadedUnplayed));
        assert_eq!(statuses.get("played-ep").map(|e| e.status), Some(BackCatalogStatus::Played));
        // Queued collapses into NeverDownloaded.
        assert_eq!(statuses.get("queued-ep").map(|e| e.status), Some(BackCatalogStatus::NeverDownloaded));
        // Missing guids mean NeverDownloaded.
        assert_eq!(statuses.get("never-touched-guid"), None);
    }

    #[test]
    fn list_history_episodes_excludes_remote_only_plays() {
        // A remote completion (sync sets `played`) mustn't appear in local
        // History.
        let (_dir, conn, sub_id) = setup();

        insert_episode(&conn, sub_id, &sample_parsed_episode("played-remotely-only"), false, None).unwrap();
        conn.execute(
            "UPDATE episodes SET played = 1, played_order = 1 WHERE guid = 'played-remotely-only'",
            [],
        )
        .unwrap();
        let remote_only_id: i64 =
            conn.query_row("SELECT id FROM episodes WHERE guid = 'played-remotely-only'", [], |row| row.get(0)).unwrap();

        insert_episode(&conn, sub_id, &sample_parsed_episode("played-locally"), false, None).unwrap();
        conn.execute(
            "UPDATE episodes SET played = 1, played_locally = 1, played_order = 2 WHERE guid = 'played-locally'",
            [],
        )
        .unwrap();
        let local_id: i64 = conn.query_row("SELECT id FROM episodes WHERE guid = 'played-locally'", [], |row| row.get(0)).unwrap();

        let history_ids: Vec<i64> = list_history_episodes(&conn).unwrap().iter().map(|e| e.id).collect();
        assert!(!history_ids.contains(&remote_only_id), "a remote-only play shouldn't appear in local History");
        assert!(history_ids.contains(&local_id), "a genuine local play should appear in local History");
    }

    #[test]
    fn back_catalog_statuses_reports_files_separately_from_played() {
        // A played episode whose file is gone, often marked played by sync,
        // offered Delete in the Episodes tab.
        let (_dir, conn, sub_id) = setup();
        insert_episode(&conn, sub_id, &sample_parsed_episode("played-deleted"), false, None).unwrap();
        insert_episode(&conn, sub_id, &sample_parsed_episode("played-kept"), false, None).unwrap();
        conn.execute("UPDATE episodes SET played = 1", []).unwrap();
        conn.execute("UPDATE episodes SET download_status = 'downloaded' WHERE guid = 'played-kept'", []).unwrap();
        let statuses = back_catalog_statuses(&conn, sub_id).unwrap();
        assert_eq!(statuses["played-deleted"].status, BackCatalogStatus::Played);
        assert!(!statuses["played-deleted"].downloaded);
        assert!(statuses["played-kept"].downloaded);
    }

    #[test]
    fn back_catalog_statuses_reflects_playback_queue_membership() {
        // is_queued follows group_queue_position, not download_status's
        // 'queued'.
        let (_dir, conn, sub_id) = setup();
        let episode_id = insert_episode(&conn, sub_id, &sample_parsed_episode("in-playback-queue"), false, None).unwrap();
        conn.execute("UPDATE episodes SET download_status = 'downloaded' WHERE id = ?1", params![episode_id]).unwrap();
        conn.execute("UPDATE episodes SET group_queue_position = 1.0 WHERE id = ?1", params![episode_id]).unwrap();

        insert_episode(&conn, sub_id, &sample_parsed_episode("not-in-playback-queue"), false, None).unwrap();

        let statuses = back_catalog_statuses(&conn, sub_id).unwrap();
        assert!(statuses.get("in-playback-queue").unwrap().is_queued);
        assert!(!statuses.get("not-in-playback-queue").unwrap().is_queued);
    }

    #[test]
    fn back_catalog_statuses_carries_the_db_own_accurate_duration() {
        // The probed duration comes through, not the feed's.
        let (_dir, conn, sub_id) = setup();
        insert_episode(&conn, sub_id, &sample_parsed_episode("downloaded-with-probed-duration"), false, None).unwrap();
        conn.execute(
            "UPDATE episodes SET download_status = 'downloaded', duration_ms = 304000 WHERE guid = 'downloaded-with-probed-duration'",
            [],
        )
        .unwrap();

        let statuses = back_catalog_statuses(&conn, sub_id).unwrap();
        assert_eq!(statuses.get("downloaded-with-probed-duration").and_then(|e| e.duration_ms), Some(304000));
    }

    #[test]
    fn insert_subscription_then_get_subscription_round_trips() {
        let (_dir, conn, _existing_sub_id) = setup();
        let new_id = insert_subscription(&conn, "https://example.com/newshow.xml", "https://example.com/newshow.xml", 1).unwrap();

        let loaded = get_subscription(&conn, new_id).unwrap().unwrap();
        assert_eq!(loaded.feed_url, "https://example.com/newshow.xml");
        // The URL is the title until a refresh.
        assert_eq!(loaded.title, "https://example.com/newshow.xml");
        assert_eq!(loaded.group_id, 1);
    }

    #[test]
    fn get_subscription_returns_none_for_missing_id() {
        let (_dir, conn, _sub_id) = setup();
        assert!(get_subscription(&conn, 9999).unwrap().is_none());
    }

    #[test]
    fn get_subscription_by_feed_url_finds_an_existing_subscription() {
        let (_dir, conn, sub_id) = setup();
        let loaded = get_subscription(&conn, sub_id).unwrap().unwrap();

        let found = get_subscription_by_feed_url(&conn, &loaded.feed_url).unwrap().unwrap();
        assert_eq!(found.id, sub_id);
    }

    #[test]
    fn get_subscription_by_feed_url_returns_none_for_an_unknown_url() {
        let (_dir, conn, _sub_id) = setup();
        assert!(get_subscription_by_feed_url(&conn, "https://example.com/never-added.xml").unwrap().is_none());
    }

    #[test]
    fn update_subscription_metadata_replaces_placeholder_title_artwork_description_and_local_path() {
        let (_dir, conn, _existing_sub_id) = setup();
        let new_id = insert_subscription(&conn, "https://example.com/newshow.xml", "https://example.com/newshow.xml", 1).unwrap();

        update_subscription_metadata(
            &conn,
            new_id,
            "The Real Show Title",
            Some("https://example.com/art.jpg"),
            Some("A show about real things."),
            Some("/tmp/phodder-downloads/artwork-1.img"),
        )
        .unwrap();

        let loaded = get_subscription(&conn, new_id).unwrap().unwrap();
        assert_eq!(loaded.title, "The Real Show Title");
        assert_eq!(loaded.artwork_url.as_deref(), Some("https://example.com/art.jpg"));
        assert_eq!(loaded.description.as_deref(), Some("A show about real things."));
        assert_eq!(loaded.artwork_local_path.as_deref(), Some("/tmp/phodder-downloads/artwork-1.img"));
    }

    #[test]
    fn a_manually_renamed_title_survives_a_later_metadata_refresh() {
        // A refresh mustn't overwrite a custom title.
        let (_dir, conn, sub_id) = setup();

        update_subscription_title(&conn, sub_id, "My Custom Name").unwrap();
        assert_eq!(get_subscription(&conn, sub_id).unwrap().unwrap().title, "My Custom Name");

        update_subscription_metadata(&conn, sub_id, "Whatever The Feed Calls Itself Now", None, None, None).unwrap();

        let loaded = get_subscription(&conn, sub_id).unwrap().unwrap();
        assert_eq!(loaded.title, "My Custom Name");
    }

    #[test]
    fn metadata_refresh_still_updates_title_normally_before_any_rename() {
        // Without a custom title, refreshes still update it.
        let (_dir, conn, sub_id) = setup();
        update_subscription_metadata(&conn, sub_id, "Whatever The Feed Calls Itself Now", None, None, None).unwrap();
        assert_eq!(get_subscription(&conn, sub_id).unwrap().unwrap().title, "Whatever The Feed Calls Itself Now");
    }

    #[test]
    fn metadata_refresh_keeps_feed_title_current_even_after_a_rename() {
        // feed_title keeps tracking the feed, so renaming can be reverted to
        // a current title.
        let (_dir, conn, sub_id) = setup();
        update_subscription_title(&conn, sub_id, "My Custom Name").unwrap();
        update_subscription_metadata(&conn, sub_id, "The Feed Renamed Itself Too", None, None, None).unwrap();

        let loaded = get_subscription(&conn, sub_id).unwrap().unwrap();
        assert_eq!(loaded.title, "My Custom Name");
        assert_eq!(loaded.feed_title.as_deref(), Some("The Feed Renamed Itself Too"));
    }

    #[test]
    fn renaming_back_to_the_feeds_own_title_clears_title_manually_set() {
        // Reverting to the feed's title resumes automatic updates.
        let (_dir, conn, sub_id) = setup();
        update_subscription_metadata(&conn, sub_id, "Original Feed Title", None, None, None).unwrap();
        update_subscription_title(&conn, sub_id, "My Custom Name").unwrap();

        update_subscription_title(&conn, sub_id, "Original Feed Title").unwrap();

        // A later feed rename applies again.
        update_subscription_metadata(&conn, sub_id, "A Further Feed Rename", None, None, None).unwrap();
        assert_eq!(get_subscription(&conn, sub_id).unwrap().unwrap().title, "A Further Feed Rename");
    }

    #[test]
    fn get_episode_detail_joins_podcast_title_and_includes_show_notes() {
        let (_dir, conn, sub_id) = setup();
        insert_episode(&conn, sub_id, &sample_parsed_episode("ep-1"), true, None).unwrap();
        conn.execute(
            "UPDATE episodes SET download_status = 'downloaded', file_size_bytes = 123456 WHERE guid = 'ep-1'",
            [],
        )
        .unwrap();
        let episode_id: i64 =
            conn.query_row("SELECT id FROM episodes WHERE guid = 'ep-1'", [], |r| r.get(0)).unwrap();

        let detail = get_episode_detail(&conn, episode_id).unwrap().unwrap();
        assert_eq!(detail.title, "Test Episode");
        assert_eq!(detail.podcast_title, "Test Show");
        assert_eq!(detail.show_notes_html.as_deref(), Some("<p>Hello <b>world</b></p>"));
        assert_eq!(detail.duration_ms, Some(60_000));
        assert_eq!(detail.download_status, "downloaded");
        assert_eq!(detail.file_size_bytes, Some(123456));
    }

    #[test]
    fn get_episode_detail_returns_none_for_missing_id() {
        let (_dir, conn, _sub_id) = setup();
        assert!(get_episode_detail(&conn, 9999).unwrap().is_none());
    }

    #[test]
    fn delete_subscription_cascades_to_its_episodes() {
        // SQLite ignores foreign-key actions unless the connection enabled
        // them.
        let (_dir, conn, sub_id) = setup();
        insert_episode(&conn, sub_id, &sample_parsed_episode("ep-1"), false, None).unwrap();
        insert_episode(&conn, sub_id, &sample_parsed_episode("ep-2"), false, None).unwrap();
        let episode_count_before: i64 =
            conn.query_row("SELECT COUNT(*) FROM episodes WHERE subscription_id = ?1", [sub_id], |r| r.get(0)).unwrap();
        assert_eq!(episode_count_before, 2);

        delete_subscription(&conn, sub_id).unwrap();

        assert!(get_subscription(&conn, sub_id).unwrap().is_none());
        let episode_count_after: i64 =
            conn.query_row("SELECT COUNT(*) FROM episodes WHERE subscription_id = ?1", [sub_id], |r| r.get(0)).unwrap();
        assert_eq!(episode_count_after, 0);
    }

    #[test]
    fn episode_file_paths_for_subscription_only_includes_downloaded_episodes() {
        let (_dir, conn, sub_id) = setup();
        insert_episode(&conn, sub_id, &sample_parsed_episode("downloaded-ep"), false, None).unwrap();
        conn.execute(
            "UPDATE episodes SET local_file_path = '/tmp/phodder-downloads/downloaded-ep.audio' WHERE guid = 'downloaded-ep'",
            [],
        )
        .unwrap();
        insert_episode(&conn, sub_id, &sample_parsed_episode("never-downloaded-ep"), false, None).unwrap();
        // Not downloaded, so no path.

        let paths = episode_file_paths_for_subscription(&conn, sub_id).unwrap();
        assert_eq!(paths, vec!["/tmp/phodder-downloads/downloaded-ep.audio".to_string()]);
    }

    #[test]
    fn insert_episode_stores_artwork_url_and_local_path() {
        let (_dir, conn, sub_id) = setup();
        let mut episode = sample_parsed_episode("ep-with-art");
        episode.artwork_url = Some("https://example.com/ep-art.jpg".to_string());

        insert_episode(&conn, sub_id, &episode, false, Some("/tmp/phodder-downloads/artwork-ep-with-art.img")).unwrap();

        let (stored_artwork_url, stored_local_path): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT artwork_url, artwork_local_path FROM episodes WHERE guid = 'ep-with-art'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored_artwork_url.as_deref(), Some("https://example.com/ep-art.jpg"));
        assert_eq!(stored_local_path.as_deref(), Some("/tmp/phodder-downloads/artwork-ep-with-art.img"));
    }

    #[test]
    fn update_subscription_settings_round_trips_toggles_filter_rules_and_expiry() {
        let (_dir, conn, sub_id) = setup();
        let rules = vec![
            crate::filter::FilterRule {
                pattern: "*bonus*".to_string(),
                target: crate::filter::MatchTarget::Title,
                action: crate::filter::Action::Block,
            },
            crate::filter::FilterRule {
                pattern: "interview".to_string(),
                target: crate::filter::MatchTarget::Both,
                action: crate::filter::Action::Allow,
            },
        ];

        update_subscription_settings(&conn, sub_id, false, true, &rules, 5).unwrap();

        let loaded = get_subscription(&conn, sub_id).unwrap().unwrap();
        assert!(!loaded.auto_download_enabled);
        assert!(loaded.notify_on_new_episodes);
        assert_eq!(loaded.download_filter_rules.len(), 2);
        assert_eq!(loaded.download_filter_rules[0].pattern, "*bonus*");
        assert_eq!(loaded.download_filter_rules[0].target, crate::filter::MatchTarget::Title);
        assert_eq!(loaded.download_filter_rules[0].action, crate::filter::Action::Block);
        assert_eq!(loaded.download_filter_rules[1].pattern, "interview");
        assert_eq!(loaded.delete_after_minutes, 5);

        // A second call overwrites the first.
        update_subscription_settings(&conn, sub_id, false, true, &rules, 100).unwrap();
        let loaded_again = get_subscription(&conn, sub_id).unwrap().unwrap();
        assert_eq!(loaded_again.delete_after_minutes, 100);
    }
}
