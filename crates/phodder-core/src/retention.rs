//! Retention: delete-after-play, delete-after-minutes, and size and
//! episode-count caps. Caps rotate: they can evict unplayed episodes.
//!
//! Every rule excludes `currently_playing` in SQL, whatever the policy.
//!
//! In-progress episodes (unplayed, with a saved position) are never evicted
//! by the age or cap rules: the startup sweep runs before the player restores
//! the last episode, so `currently_playing` doesn't cover it.
//!
//! delete-after-play and the caps exclude queued episodes.
//! delete-after-minutes doesn't: a short expiry (a daily news show given 2
//! days) exists to de-queue and delete what went unheard; see
//! `older_than_minutes`.

use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    pub delete_after_play: bool,
    // Required on every subscription (schema NOT NULL DEFAULT); there's no
    // global default.
    pub delete_after_minutes: i64,
    pub size_limit_bytes: Option<i64>,
    pub episode_count_limit: Option<i64>,
}

/// `delete_after_play`, `size_limit_bytes` and `episode_count_limit` fall
/// back to their `default_*` settings when NULL on the subscription;
/// `delete_after_minutes` always has its own value.
pub fn resolve_policy(conn: &Connection, subscription_id: i64) -> rusqlite::Result<RetentionPolicy> {
    let (delete_after_play, delete_after_minutes, size_limit_bytes, episode_count_limit): (
        Option<i64>,
        i64,
        Option<i64>,
        Option<i64>,
    ) = conn.query_row(
        "SELECT delete_after_play, delete_after_minutes, size_limit_bytes, episode_count_limit
         FROM subscriptions WHERE id = ?1",
        params![subscription_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;

    Ok(RetentionPolicy {
        delete_after_play: delete_after_play
            .map(|v| v != 0)
            .unwrap_or_else(|| get_setting_bool(conn, "default_delete_after_play").unwrap_or(false)),
        delete_after_minutes,
        size_limit_bytes: size_limit_bytes.or_else(|| get_setting_i64(conn, "default_size_limit_bytes")),
        episode_count_limit: episode_count_limit
            .or_else(|| get_setting_i64(conn, "default_episode_count_limit")),
    })
}

/// Played episodes when `delete_after_play` is on. `evictable_episodes`
/// checks the policy first.
fn played_and_delete_after_play(
    conn: &Connection,
    subscription_id: i64,
    currently_playing: Option<i64>,
) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM episodes
         WHERE subscription_id = ?1
           AND played = 1
           AND download_status = 'downloaded'
           AND group_queue_position IS NULL
           AND id != COALESCE(?2, -1)",
    )?;
    let result = stmt
        .query_map(params![subscription_id, currently_playing], |row| row.get(0))?
        .collect();
    result
}

/// Episodes published more than `minutes` ago, counted from `publish_date`,
/// not `downloaded_at`: the setting means "how long after release", and the
/// two dates can differ widely for backfilled episodes. Minutes so short
/// expiries such as 5 minutes work.
///
/// Covers 'downloaded' and 'failed'; a failed download otherwise stayed in
/// Library forever, since nothing else re-evaluates it. 'queued' is
/// re-checked by every download pass, and 'downloading' would kill an
/// in-flight download.
///
/// Doesn't exclude queued episodes; see the module doc.
fn older_than_minutes(
    conn: &Connection,
    subscription_id: i64,
    minutes: i64,
    currently_playing: Option<i64>,
) -> rusqlite::Result<Vec<i64>> {
    let cutoff = (Utc::now() - Duration::minutes(minutes)).to_rfc3339();
    // Failed downloads age out too: nothing else re-evaluates them.
    let mut stmt = conn.prepare(
        "SELECT id FROM episodes
         WHERE subscription_id = ?1
           AND download_status IN ('downloaded', 'failed')
           AND manually_downloaded = 0
           AND NOT (played = 0 AND playback_position_ms > 0)
           AND id != COALESCE(?2, -1)
           AND publish_date <= ?3",
    )?;
    let result: Vec<i64> = stmt
        .query_map(params![subscription_id, currently_playing, cutoff], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(result)
}

/// Brings a subscription under its size and episode-count caps, evicting the
/// oldest downloads first regardless of played state until both are
/// satisfied. A `None` cap is ignored.
fn over_caps(
    conn: &Connection,
    subscription_id: i64,
    size_limit_bytes: Option<i64>,
    episode_count_limit: Option<i64>,
    currently_playing: Option<i64>,
) -> rusqlite::Result<Vec<i64>> {
    if size_limit_bytes.is_none() && episode_count_limit.is_none() {
        return Ok(vec![]);
    }

    let mut stmt = conn.prepare(
        "SELECT id, COALESCE(file_size_bytes, 0) FROM episodes
         WHERE subscription_id = ?1
           AND download_status = 'downloaded'
           AND manually_downloaded = 0
           AND group_queue_position IS NULL
           AND NOT (played = 0 AND playback_position_ms > 0)
           AND id != COALESCE(?2, -1)
         ORDER BY downloaded_at ASC", // oldest first -- these go first
    )?;
    let rows: Vec<(i64, i64)> = stmt
        .query_map(params![subscription_id, currently_playing], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut remaining_count = rows.len() as i64;
    let mut remaining_size: i64 = rows.iter().map(|(_, size)| size).sum();

    let mut to_evict = Vec::new();
    for (id, size) in rows {
        let over_count = episode_count_limit.is_some_and(|limit| remaining_count > limit);
        let over_size = size_limit_bytes.is_some_and(|limit| remaining_size > limit);
        if !over_count && !over_size {
            break; // Both satisfied; nothing newer is evicted.
        }
        to_evict.push(id);
        remaining_count -= 1;
        remaining_size -= size;
    }
    Ok(to_evict)
}

/// Everything a subscription's policy makes evictable, deduplicated.
pub fn evictable_episodes(
    conn: &Connection,
    subscription_id: i64,
    currently_playing: Option<i64>,
) -> rusqlite::Result<Vec<i64>> {
    let policy = resolve_policy(conn, subscription_id)?;
    let mut evict = BTreeSet::new();

    if policy.delete_after_play {
        evict.extend(played_and_delete_after_play(conn, subscription_id, currently_playing)?);
    }
    evict.extend(older_than_minutes(conn, subscription_id, policy.delete_after_minutes, currently_playing)?);
    evict.extend(over_caps(
        conn,
        subscription_id,
        policy.size_limit_bytes,
        policy.episode_count_limit,
        currently_playing,
    )?);

    Ok(evict.into_iter().collect())
}

pub fn list_subscription_ids(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM subscriptions")?;
    let result = stmt.query_map([], |row| row.get(0))?.collect();
    result
}

/// The file to remove before `mark_episode_deleted` clears the bookkeeping.
pub fn episode_local_file_path(conn: &Connection, episode_id: i64) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT local_file_path FROM episodes WHERE id = ?1",
        params![episode_id],
        |row| row.get(0),
    )
    .optional()
    .map(|opt| opt.flatten())
}

/// Clears an episode's download state and queue position after its file is
/// deleted; touches only the database. The queue position matters for
/// `older_than_minutes`, the one path that evicts queued episodes; elsewhere
/// it's already NULL.
pub fn mark_episode_deleted(conn: &Connection, episode_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE episodes
         SET download_status = 'none',
             local_file_path = NULL,
             file_size_bytes = NULL,
             downloaded_at = NULL,
             group_queue_position = NULL
         WHERE id = ?1",
        params![episode_id],
    )?;
    Ok(())
}

fn get_setting_bool(conn: &Connection, key: &str) -> Option<bool> {
    crate::settings::get_setting_bool(conn, key)
}

fn get_setting_i64(conn: &Connection, key: &str) -> Option<i64> {
    crate::settings::get_setting_i64(conn, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{migrate, open_connection};

    /// Keep the TempDir alive (`_dir`) or the file is deleted out from under
    /// the connection.
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

    fn insert_episode(
        conn: &Connection,
        subscription_id: i64,
        guid: &str,
        played: bool,
        downloaded_days_ago: i64,
        size_bytes: i64,
        queued: bool,
    ) -> i64 {
        let downloaded_at = (Utc::now() - Duration::days(downloaded_days_ago)).to_rfc3339();
        conn.execute(
            "INSERT INTO episodes
                (subscription_id, guid, title, audio_url, publish_date, download_status,
                 played, downloaded_at, file_size_bytes, group_queue_position)
             VALUES (?1, ?2, ?2, 'https://example.com/ep.mp3', ?3, 'downloaded', ?4, ?3, ?5, ?6)",
            params![
                subscription_id,
                guid,
                downloaded_at,
                played as i64,
                size_bytes,
                if queued { Some(1.0) } else { None::<f64> },
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn delete_after_play_only_evicts_played_unqueued_episodes() {
        let (_dir, conn, sub_id) = setup();
        let played = insert_episode(&conn, sub_id, "played", true, 1, 1000, false);
        let _unplayed = insert_episode(&conn, sub_id, "unplayed", false, 1, 1000, false);
        let _queued_played = insert_episode(&conn, sub_id, "queued", true, 1, 1000, true);

        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        // delete_after_play defaults to off.
        assert!(evictable.is_empty());

        conn.execute(
            "UPDATE subscriptions SET delete_after_play = 1 WHERE id = ?1",
            params![sub_id],
        )
        .unwrap();
        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert_eq!(evictable, vec![played]);
    }

    #[test]
    fn currently_playing_is_never_evicted_even_if_played_flag_is_set() {
        let (_dir, conn, sub_id) = setup();
        let playing = insert_episode(&conn, sub_id, "playing", true, 30, 1000, false);
        conn.execute(
            "UPDATE subscriptions SET delete_after_play = 1, delete_after_minutes = 1440 WHERE id = ?1",
            params![sub_id],
        )
        .unwrap();

        let evictable = evictable_episodes(&conn, sub_id, Some(playing)).unwrap();
        assert!(evictable.is_empty(), "currently-playing episode must never be evicted");
    }

    #[test]
    fn delete_after_minutes_counts_from_publish_date() {
        let (_dir, conn, sub_id) = setup();
        let old = insert_episode(&conn, sub_id, "old", false, 10, 1000, false);
        let _recent = insert_episode(&conn, sub_id, "recent", false, 1, 1000, false);
        // 5 days in minutes.
        conn.execute("UPDATE subscriptions SET delete_after_minutes = 7200 WHERE id = ?1", params![sub_id])
            .unwrap();

        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert_eq!(evictable, vec![old]);
    }

    #[test]
    fn delete_after_minutes_uses_publish_date_not_download_date() {
        // insert_episode sets publish_date and downloaded_at to the same
        // value, so only this test tells them apart: one episode published
        // long ago and downloaded recently, one the reverse.
        let (_dir, conn, sub_id) = setup();
        let published_long_ago_downloaded_recently = {
            let publish_date = (Utc::now() - Duration::days(10)).to_rfc3339();
            let downloaded_at = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO episodes
                    (subscription_id, guid, title, audio_url, publish_date, download_status,
                     played, downloaded_at, file_size_bytes)
                 VALUES (?1, 'old-publish-new-download', 'ep', 'https://example.com/ep.mp3', ?2, 'downloaded', 0, ?3, 1000)",
                params![sub_id, publish_date, downloaded_at],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let published_recently_downloaded_long_ago = {
            let publish_date = Utc::now().to_rfc3339();
            let downloaded_at = (Utc::now() - Duration::days(10)).to_rfc3339();
            conn.execute(
                "INSERT INTO episodes
                    (subscription_id, guid, title, audio_url, publish_date, download_status,
                     played, downloaded_at, file_size_bytes)
                 VALUES (?1, 'new-publish-old-download', 'ep', 'https://example.com/ep.mp3', ?2, 'downloaded', 0, ?3, 1000)",
                params![sub_id, publish_date, downloaded_at],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        // 5 days in minutes.
        conn.execute("UPDATE subscriptions SET delete_after_minutes = 7200 WHERE id = ?1", params![sub_id])
            .unwrap();

        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert_eq!(
            evictable,
            vec![published_long_ago_downloaded_recently],
            "only the episode published (not downloaded) more than 5 days ago should be evictable"
        );
        assert!(!evictable.contains(&published_recently_downloaded_long_ago));
    }

    #[test]
    fn delete_after_minutes_supports_short_expiries_like_five_minutes() {
        // A little over 5 minutes old, under a 5-minute expiry.
        let (_dir, conn, sub_id) = setup();
        let old_enough = insert_episode(&conn, sub_id, "old-enough", false, 1, 1000, false);
        conn.execute("UPDATE subscriptions SET delete_after_minutes = 5 WHERE id = ?1", params![sub_id])
            .unwrap();

        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert_eq!(evictable, vec![old_enough]);
    }

    #[test]
    fn delete_after_minutes_evicts_played_episodes_too() {
        // older_than_minutes ignores played status; only delete_after_play
        // checks it.
        let (_dir, conn, sub_id) = setup();
        let old_and_played = insert_episode(&conn, sub_id, "old-played", true, 10, 1000, false);
        let old_and_unplayed = insert_episode(&conn, sub_id, "old-unplayed", false, 10, 1000, false);
        let recent_and_played = insert_episode(&conn, sub_id, "recent-played", true, 1, 1000, false);
        conn.execute("UPDATE subscriptions SET delete_after_minutes = 7200 WHERE id = ?1", params![sub_id])
            .unwrap();

        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert!(evictable.contains(&old_and_played), "an old episode should be evictable whether played or not");
        assert!(evictable.contains(&old_and_unplayed));
        assert!(!evictable.contains(&recent_and_played), "a recent episode stays regardless of played status");
    }

    #[test]
    fn manually_downloaded_episodes_are_exempt_from_age_based_eviction() {
        // A manually downloaded episode is exempt from age-based eviction;
        // the sweep used to delete it again right away.
        let (_dir, conn, sub_id) = setup();
        let old_manual = insert_episode(&conn, sub_id, "old-manual", false, 10, 1000, false);
        let old_auto = insert_episode(&conn, sub_id, "old-auto", false, 10, 1000, false);
        conn.execute("UPDATE episodes SET manually_downloaded = 1 WHERE id = ?1", params![old_manual]).unwrap();
        conn.execute("UPDATE subscriptions SET delete_after_minutes = 7200 WHERE id = ?1", params![sub_id]).unwrap();

        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert!(!evictable.contains(&old_manual), "a manually-downloaded episode should never be evicted by age");
        assert!(evictable.contains(&old_auto), "an old, auto-downloaded episode should still be evicted normally");
    }

    #[test]
    fn manually_downloaded_episodes_are_exempt_from_cap_based_eviction() {
        // The same exemption in over_caps, which has its own query.
        let (_dir, conn, sub_id) = setup();
        let manual = insert_episode(&conn, sub_id, "manual", false, 5, 1000, false);
        let auto = insert_episode(&conn, sub_id, "auto", false, 4, 1000, false);
        conn.execute("UPDATE episodes SET manually_downloaded = 1 WHERE id = ?1", params![manual]).unwrap();
        conn.execute("UPDATE subscriptions SET episode_count_limit = 0 WHERE id = ?1", params![sub_id]).unwrap();

        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert!(!evictable.contains(&manual), "a manually-downloaded episode should never be evicted by size/count caps");
        assert!(evictable.contains(&auto), "an auto-downloaded episode should still be evicted normally to satisfy the cap");
    }

    #[test]
    fn delete_after_minutes_evicts_queued_episodes_unlike_every_other_eviction_path() {
        // Expiry de-queues and deletes queued episodes, unlike the other
        // rules.
        let (_dir, conn, sub_id) = setup();
        let expired_and_queued = insert_episode(&conn, sub_id, "expired-queued", false, 10, 1000, true);
        conn.execute("UPDATE subscriptions SET delete_after_minutes = 2880 WHERE id = ?1", params![sub_id])
            .unwrap();

        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert_eq!(evictable, vec![expired_and_queued]);
    }

    #[test]
    fn delete_after_minutes_also_evicts_failed_episodes() {
        // Old 'failed' episodes are evicted; 'queued' and 'downloading'
        // aren't.
        let (_dir, conn, sub_id) = setup();
        let old_failed = insert_episode(&conn, sub_id, "old-failed", false, 10, 1000, false);
        conn.execute("UPDATE episodes SET download_status = 'failed' WHERE id = ?1", params![old_failed]).unwrap();
        let old_queued = insert_episode(&conn, sub_id, "old-queued", false, 10, 1000, false);
        conn.execute("UPDATE episodes SET download_status = 'queued' WHERE id = ?1", params![old_queued]).unwrap();
        let old_downloading = insert_episode(&conn, sub_id, "old-downloading", false, 10, 1000, false);
        conn.execute("UPDATE episodes SET download_status = 'downloading' WHERE id = ?1", params![old_downloading]).unwrap();
        conn.execute("UPDATE subscriptions SET delete_after_minutes = 2880 WHERE id = ?1", params![sub_id]).unwrap();

        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert!(evictable.contains(&old_failed), "an old, failed download should be evicted the same as an old, downloaded one");
        assert!(!evictable.contains(&old_queued), "still-queued episodes are covered by the download pass's own re-check, not this eviction path");
        assert!(!evictable.contains(&old_downloading), "an actively in-progress download must never be evicted out from under itself");
    }

    #[test]
    fn delete_after_minutes_still_never_evicts_the_currently_playing_episode() {
        // currently_playing wins even for an episode that's queued and
        // expired.
        let (_dir, conn, sub_id) = setup();
        let playing_and_expired = insert_episode(&conn, sub_id, "playing-expired", false, 10, 1000, true);
        conn.execute("UPDATE subscriptions SET delete_after_minutes = 2880 WHERE id = ?1", params![sub_id])
            .unwrap();

        let evictable = evictable_episodes(&conn, sub_id, Some(playing_and_expired)).unwrap();
        assert!(evictable.is_empty(), "currently-playing episode must never be evicted, even if queued and expired");
    }

    #[test]
    fn mark_episode_deleted_clears_queue_position_too() {
        // End to end: evicted and de-queued.
        let (_dir, conn, sub_id) = setup();
        let episode_id = insert_episode(&conn, sub_id, "expired-queued", false, 10, 1000, true);

        mark_episode_deleted(&conn, episode_id).unwrap();

        let queue_position: Option<f64> = conn
            .query_row("SELECT group_queue_position FROM episodes WHERE id = ?1", [episode_id], |r| r.get(0))
            .unwrap();
        assert!(queue_position.is_none(), "episode should be de-queued, not just have its file cleared");
    }

    #[test]
    fn size_cap_evicts_oldest_first_including_unplayed() {
        let (_dir, conn, sub_id) = setup();
        // Three 500MB episodes under an 800MB cap: evicting continues until
        // one is left.
        let oldest = insert_episode(&conn, sub_id, "oldest", false, 3, 500_000_000, false);
        let middle = insert_episode(&conn, sub_id, "middle", false, 2, 500_000_000, false);
        let _newest = insert_episode(&conn, sub_id, "newest", false, 1, 500_000_000, false);
        conn.execute(
            "UPDATE subscriptions SET size_limit_bytes = 800000000 WHERE id = ?1",
            params![sub_id],
        )
        .unwrap();

        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert_eq!(evictable, vec![oldest, middle], "true rotation evicts unplayed episodes too");
    }

    #[test]
    fn episode_count_cap_keeps_only_the_n_most_recent() {
        let (_dir, conn, sub_id) = setup();
        let oldest = insert_episode(&conn, sub_id, "e1", false, 5, 100, false);
        let _e2 = insert_episode(&conn, sub_id, "e2", false, 4, 100, false);
        let _e3 = insert_episode(&conn, sub_id, "e3", false, 3, 100, false);
        let _e4 = insert_episode(&conn, sub_id, "e4", false, 2, 100, false);
        let _e5 = insert_episode(&conn, sub_id, "e5", false, 1, 100, false);
        conn.execute(
            "UPDATE subscriptions SET episode_count_limit = 4 WHERE id = ?1",
            params![sub_id],
        )
        .unwrap();

        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert_eq!(evictable, vec![oldest], "only the single oldest should be evicted to get to 4");
    }

    #[test]
    fn recent_episode_is_not_evicted_under_the_default_expiry() {
        // The schema default (7 days) doesn't evict a recent episode.
        let (_dir, conn, sub_id) = setup();
        insert_episode(&conn, sub_id, "e1", true, 1, 1_000_000_000, false);
        let evictable = evictable_episodes(&conn, sub_id, None).unwrap();
        assert!(evictable.is_empty());
    }

    #[test]
    fn episode_local_file_path_returns_none_for_undownloaded_episode() {
        let (_dir, conn, sub_id) = setup();
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date)
             VALUES (?1, 'g', 't', 'https://example.com/e.mp3', '2026-01-01T00:00:00Z')",
            params![sub_id],
        )
        .unwrap();
        let episode_id = conn.last_insert_rowid();
        assert_eq!(episode_local_file_path(&conn, episode_id).unwrap(), None);
    }

    #[test]
    fn episode_local_file_path_returns_path_when_set() {
        let (_dir, conn, sub_id) = setup();
        let ep = insert_episode(&conn, sub_id, "e", false, 1, 1000, false);
        conn.execute(
            "UPDATE episodes SET local_file_path = '/tmp/example.mp3' WHERE id = ?1",
            params![ep],
        )
        .unwrap();
        assert_eq!(
            episode_local_file_path(&conn, ep).unwrap(),
            Some("/tmp/example.mp3".to_string())
        );
    }

    #[test]
    fn mark_episode_deleted_clears_download_bookkeeping() {
        let (_dir, conn, sub_id) = setup();
        let ep = insert_episode(&conn, sub_id, "e", false, 1, 1000, false);
        mark_episode_deleted(&conn, ep).unwrap();

        let (status, path, size, downloaded_at): (String, Option<String>, Option<i64>, Option<String>) =
            conn.query_row(
                "SELECT download_status, local_file_path, file_size_bytes, downloaded_at FROM episodes WHERE id = ?1",
                params![ep],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(status, "none");
        assert_eq!(path, None);
        assert_eq!(size, None);
        assert_eq!(downloaded_at, None);
    }

    #[test]
    fn list_subscription_ids_returns_all_subscriptions() {
        let (_dir, conn, sub_id) = setup();
        conn.execute(
            "INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/feed2', 'Show 2', 1)",
            [],
        )
        .unwrap();
        let second_id = conn.last_insert_rowid();
        let mut ids = list_subscription_ids(&conn).unwrap();
        ids.sort();
        let mut expected = vec![sub_id, second_id];
        expected.sort();
        assert_eq!(ids, expected);
    }
    #[test]
    fn in_progress_episodes_are_exempt_from_age_and_cap_eviction() {
        let (_dir, conn, sub) = setup();
        conn.execute("UPDATE subscriptions SET delete_after_minutes = 60, episode_count_limit = 0 WHERE id = ?1", [sub]).unwrap();
        let in_progress = insert_episode(&conn, sub, "in-progress", false, 30, 1000, true);
        let unqueued_in_progress = insert_episode(&conn, sub, "in-progress-unqueued", false, 30, 1000, false);
        let untouched = insert_episode(&conn, sub, "untouched", false, 30, 1000, true);
        conn.execute("UPDATE episodes SET playback_position_ms = 95000 WHERE id IN (?1, ?2)", [in_progress, unqueued_in_progress]).unwrap();

        let evicted = evictable_episodes(&conn, sub, None).unwrap();
        assert!(!evicted.contains(&in_progress));
        assert!(!evicted.contains(&unqueued_in_progress));
        assert!(evicted.contains(&untouched), "unstarted expired episodes still go");
    }
}
