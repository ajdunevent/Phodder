//! The group-priority queue. "What plays next" is one query rather than a
//! maintained table (see schema.sql), shared by playback and the Queue tab so
//! the two can't disagree.

use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub struct NextEpisode {
    pub episode_id: i64,
    pub subscription_id: i64,
    pub local_file_path: String,
}

/// Manual played toggle. Marking played dequeues the episode and lists it in
/// History; unmarking removes it from History and keeps the position.
pub fn set_played(conn: &Connection, episode_id: i64, played: bool) -> rusqlite::Result<()> {
    if played {
        conn.execute(
            "UPDATE episodes
             SET played_order = CASE WHEN played = 0 OR played_order IS NULL
                                     THEN (SELECT COALESCE(MAX(played_order), 0) + 1 FROM episodes)
                                     ELSE played_order END,
                 played = 1,
                 played_locally = 1
             WHERE id = ?1",
            [episode_id],
        )?;
        dequeue_episode(conn, episode_id)
    } else {
        conn.execute(
            "UPDATE episodes SET played = 0, played_locally = 0, played_order = NULL WHERE id = ?1",
            [episode_id],
        )?;
        Ok(())
    }
}

/// What plays next: the front of the highest-priority non-empty group.
/// `None` when nothing is queued.
pub fn next_episode(conn: &Connection) -> rusqlite::Result<Option<NextEpisode>> {
    next_episode_excluding(conn, &[])
}

/// Callers advancing past the loaded episode must exclude it: an
/// interrupted or skipped episode stays queued at the front of its
/// group, so without the exclusion Next() reloads the same episode.
pub fn next_episode_excluding(conn: &Connection, exclude: &[i64]) -> rusqlite::Result<Option<NextEpisode>> {
    let exclude_json = format!("[{}]", exclude.iter().map(i64::to_string).collect::<Vec<_>>().join(","));
    conn.query_row(
        "SELECT e.id, e.subscription_id, e.local_file_path
         FROM episodes e
         JOIN subscriptions s ON s.id = e.subscription_id
         JOIN groups g ON g.id = s.group_id
         WHERE e.group_queue_position IS NOT NULL
           AND e.local_file_path IS NOT NULL
           AND e.id NOT IN (SELECT value FROM json_each(?1))
         ORDER BY g.sort_order ASC, e.group_queue_position ASC
         LIMIT 1",
        [exclude_json],
        |row| {
            Ok(NextEpisode {
                episode_id: row.get(0)?,
                subscription_id: row.get(1)?,
                local_file_path: row.get(2)?,
            })
        },
    )
    .optional()
}

/// Removes the episode from the queue. Finishing always dequeues; the
/// delete-after-play setting decides only whether the file goes too.
pub fn dequeue_episode(conn: &Connection, episode_id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE episodes SET group_queue_position = NULL WHERE id = ?1", [episode_id])?;
    Ok(())
}

/// Queued episodes that weren't manually downloaded, for re-checking against
/// changed filter rules. Manual choices are never undone by filters.
pub fn queued_non_manual_episodes_for_subscription(
    conn: &Connection,
    subscription_id: i64,
) -> rusqlite::Result<Vec<(i64, String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, show_notes_html FROM episodes
         WHERE subscription_id = ?1 AND group_queue_position IS NOT NULL AND manually_downloaded = 0",
    )?;
    let rows = stmt.query_map(params![subscription_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    rows.collect()
}

/// Moves an episode to the front of its group: where an interrupted episode
/// goes when something else is played. One fractional-index update. A no-op
/// when the episode isn't queued.
pub fn move_episode_to_front_of_group(conn: &Connection, episode_id: i64) -> rusqlite::Result<()> {
    let group_id: Option<i64> = conn
        .query_row(
            "SELECT s.group_id FROM episodes e
             JOIN subscriptions s ON s.id = e.subscription_id
             WHERE e.id = ?1 AND e.group_queue_position IS NOT NULL",
            [episode_id],
            |row| row.get(0),
        )
        .optional()?;

    let Some(group_id) = group_id else {
        return Ok(());
    };

    let current_min: Option<f64> = conn.query_row(
        "SELECT MIN(e.group_queue_position) FROM episodes e
         JOIN subscriptions s ON s.id = e.subscription_id
         WHERE s.group_id = ?1 AND e.group_queue_position IS NOT NULL AND e.id != ?2",
        rusqlite::params![group_id, episode_id],
        |row| row.get(0),
    )?;

    let new_position = midpoint_sort_value(None, current_min);
    conn.execute("UPDATE episodes SET group_queue_position = ?1 WHERE id = ?2", rusqlite::params![new_position, episode_id])?;
    Ok(())
}

/// Below this, an interrupted episode counts as not started and keeps its
/// place in the queue. A crash or restore right after loading leaves a few
/// milliseconds of progress, which used to override the user's order.
pub const STARTED_THRESHOLD_MS: i64 = 30_000;

/// When `now_playing` interrupts `interrupted`: the newly played episode
/// moves to the front of its group, and a started interrupted one goes
/// right after it when they share a group (front of its own group
/// otherwise), so it resumes next. Putting it first instead sent it back
/// above an episode the user had just dragged over it. No-ops for episodes
/// that aren't queued.
pub fn place_after_interruption(conn: &Connection, interrupted: i64, now_playing: i64) -> rusqlite::Result<()> {
    if interrupted == now_playing {
        return Ok(());
    }
    let interrupted_position: i64 =
        conn.query_row("SELECT playback_position_ms FROM episodes WHERE id = ?1", [interrupted], |row| row.get(0)).optional()?.unwrap_or(0);
    let started = interrupted_position >= STARTED_THRESHOLD_MS;
    // reorder_queued_episode would queue an unqueued episode, so membership
    // is checked here.
    let queued_group = |episode_id: i64| -> rusqlite::Result<Option<i64>> {
        conn.query_row(
            "SELECT s.group_id FROM episodes e JOIN subscriptions s ON s.id = e.subscription_id
             WHERE e.id = ?1 AND e.group_queue_position IS NOT NULL",
            [episode_id],
            |row| row.get(0),
        )
        .optional()
    };
    let interrupted_group = queued_group(interrupted)?;
    let playing_group = queued_group(now_playing)?;
    if playing_group.is_some() {
        move_episode_to_front_of_group(conn, now_playing)?;
    }
    match (interrupted_group, playing_group) {
        _ if !started => Ok(()),
        (Some(a), Some(b)) if a == b => reorder_queued_episode(conn, interrupted, now_playing).map_err(|e| match e {
            ReorderError::Sqlite(e) => e,
            ReorderError::DifferentGroups => unreachable!("both are queued in group {a}"),
        }),
        (Some(_), _) => move_episode_to_front_of_group(conn, interrupted),
        (None, _) => Ok(()),
    }
}

#[derive(Debug)]
pub enum ReorderError {
    /// Moving an episode out of its group is refused here, not only prevented
    /// by the UI.
    DifferentGroups,
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for ReorderError {
    fn from(e: rusqlite::Error) -> Self {
        ReorderError::Sqlite(e)
    }
}

/// Moves the dragged episode to just after the target, within their shared
/// group. A no-op when they're the same episode.
pub fn reorder_queued_episode(conn: &Connection, dragged_episode_id: i64, target_episode_id: i64) -> Result<(), ReorderError> {
    if dragged_episode_id == target_episode_id {
        return Ok(());
    }

    let dragged_group: Option<i64> = conn
        .query_row(
            "SELECT s.group_id FROM episodes e JOIN subscriptions s ON s.id = e.subscription_id WHERE e.id = ?1",
            [dragged_episode_id],
            |row| row.get(0),
        )
        .optional()?;
    let target_group: Option<i64> = conn
        .query_row(
            "SELECT s.group_id FROM episodes e JOIN subscriptions s ON s.id = e.subscription_id WHERE e.id = ?1",
            [target_episode_id],
            |row| row.get(0),
        )
        .optional()?;

    if dragged_group.is_none() || dragged_group != target_group {
        return Err(ReorderError::DifferentGroups);
    }
    let group_id = dragged_group.expect("checked non-None above");

    let target_position: f64 = conn.query_row("SELECT group_queue_position FROM episodes WHERE id = ?1", [target_episode_id], |row| row.get(0))?;
    let next_position: Option<f64> = conn
        .query_row(
            "SELECT MIN(e.group_queue_position) FROM episodes e
             JOIN subscriptions s ON s.id = e.subscription_id
             WHERE s.group_id = ?1 AND e.group_queue_position > ?2 AND e.id != ?3",
            rusqlite::params![group_id, target_position, dragged_episode_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();

    let new_position = midpoint_sort_value(Some(target_position), next_position);
    conn.execute("UPDATE episodes SET group_queue_position = ?1 WHERE id = ?2", rusqlite::params![new_position, dragged_episode_id])?;
    Ok(())
}

/// Moves an episode to the front of its group when dropped on the group's
/// header (dropping on a row can only place it after something). Refused with
/// DifferentGroups when it isn't in `expected_group_id`.
pub fn move_episode_to_front_of_group_if_member(conn: &Connection, episode_id: i64, expected_group_id: i64) -> Result<(), ReorderError> {
    let actual_group: Option<i64> = conn
        .query_row(
            "SELECT s.group_id FROM episodes e JOIN subscriptions s ON s.id = e.subscription_id WHERE e.id = ?1",
            [episode_id],
            |row| row.get(0),
        )
        .optional()?;
    if actual_group != Some(expected_group_id) {
        return Err(ReorderError::DifferentGroups);
    }
    move_episode_to_front_of_group(conn, episode_id)?;
    Ok(())
}

/// Adds an episode to the back of its group's queue; every completed download
/// calls this. A no-op when already queued, so re-downloading doesn't move
/// it.
pub fn enqueue_episode(conn: &Connection, episode_id: i64) -> rusqlite::Result<()> {
    let group_id: Option<i64> = conn
        .query_row(
            "SELECT s.group_id FROM episodes e
             JOIN subscriptions s ON s.id = e.subscription_id
             WHERE e.id = ?1 AND e.group_queue_position IS NULL",
            [episode_id],
            |row| row.get(0),
        )
        .optional()?;

    let Some(group_id) = group_id else {
        return Ok(());
    };

    let current_max: Option<f64> = conn.query_row(
        "SELECT MAX(e.group_queue_position) FROM episodes e
         JOIN subscriptions s ON s.id = e.subscription_id
         WHERE s.group_id = ?1 AND e.group_queue_position IS NOT NULL",
        [group_id],
        |row| row.get(0),
    )?;

    let new_position = midpoint_sort_value(current_max, None);
    conn.execute("UPDATE episodes SET group_queue_position = ?1 WHERE id = ?2", rusqlite::params![new_position, episode_id])?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct QueuedEpisodeSummary {
    pub episode_id: i64,
    pub title: String,
    pub podcast_title: String,
    pub group_id: i64,
    pub group_name: String,
    /// Tints the group header in the Queue tab; see schema.sql.
    pub group_color: Option<String>,
    /// The remaining fields mirror EpisodeSummary (Library's rows). No
    /// download_status: every queued episode is downloaded.
    pub file_size_bytes: Option<i64>,
    pub duration_ms: Option<i64>,
    pub publish_date: String,
    pub show_notes_html: Option<String>,
    pub artwork_local_path: Option<String>,
    pub subscription_artwork_local_path: Option<String>,
}

/// The whole queue in the order `next_episode` uses, with display fields.
pub fn list_queue(conn: &Connection) -> rusqlite::Result<Vec<QueuedEpisodeSummary>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.title, s.title, g.id, g.name, g.color,
                e.file_size_bytes, e.duration_ms, e.publish_date, e.show_notes_html,
                e.artwork_local_path, s.artwork_local_path
         FROM episodes e
         JOIN subscriptions s ON s.id = e.subscription_id
         JOIN groups g ON g.id = s.group_id
         WHERE e.group_queue_position IS NOT NULL
         ORDER BY g.sort_order ASC, e.group_queue_position ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(QueuedEpisodeSummary {
            episode_id: row.get(0)?,
            title: row.get(1)?,
            podcast_title: row.get(2)?,
            group_id: row.get(3)?,
            group_name: row.get(4)?,
            group_color: row.get(5)?,
            file_size_bytes: row.get(6)?,
            duration_ms: row.get(7)?,
            publish_date: row.get(8)?,
            show_notes_html: row.get(9)?,
            artwork_local_path: row.get(10)?,
            subscription_artwork_local_path: row.get(11)?,
        })
    })?;
    rows.collect()
}

/// A fractional sort value between two neighbors, so a reorder is one UPDATE.
/// `None` means the start or end.
///
/// Repeated reorders in one spot halve the gap each time; a renumbering pass
/// would fix that if it ever matters.
pub fn midpoint_sort_value(before: Option<f64>, after: Option<f64>) -> f64 {
    match (before, after) {
        (None, None) => 1.0,
        (None, Some(after)) => after - 1.0,
        (Some(before), None) => before + 1.0,
        (Some(before), Some(after)) => (before + after) / 2.0,
    }
}

#[derive(Debug, Clone)]
pub struct EpisodeDisplayInfo {
    pub title: String,
    pub podcast_title: String,
    pub artwork_url: Option<String>,
    pub duration_ms: Option<i64>,
    /// The cached artwork file for display: the episode's, else the show's.
    /// `artwork_url` is kept for MPRIS, which accepts remote URIs.
    pub artwork_local_path: Option<String>,
}

/// Display metadata for MPRIS and the player.
pub fn episode_display_info(
    conn: &Connection,
    episode_id: i64,
) -> rusqlite::Result<Option<EpisodeDisplayInfo>> {
    conn.query_row(
        "SELECT e.title, s.title, s.artwork_url, e.duration_ms,
                e.artwork_local_path, s.artwork_local_path
         FROM episodes e
         JOIN subscriptions s ON s.id = e.subscription_id
         WHERE e.id = ?1",
        [episode_id],
        |row| {
            let episode_artwork_local_path: Option<String> = row.get(4)?;
            let subscription_artwork_local_path: Option<String> = row.get(5)?;
            Ok(EpisodeDisplayInfo {
                title: row.get(0)?,
                podcast_title: row.get(1)?,
                artwork_url: row.get(2)?,
                duration_ms: row.get(3)?,
                artwork_local_path: episode_artwork_local_path.or(subscription_artwork_local_path),
            })
        },
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midpoint_between_two_values() {
        assert_eq!(midpoint_sort_value(Some(1.0), Some(2.0)), 1.5);
    }

    #[test]
    fn midpoint_at_start_and_end() {
        assert_eq!(midpoint_sort_value(None, Some(5.0)), 4.0);
        assert_eq!(midpoint_sort_value(Some(5.0), None), 6.0);
        assert_eq!(midpoint_sort_value(None, None), 1.0);
    }

    #[test]
    fn episode_display_info_joins_subscription_title_and_artwork() {
        use crate::db::{migrate, open_connection};

        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO subscriptions (feed_url, title, artwork_url, group_id)
             VALUES ('https://example.com/feed', 'Great Show', 'https://example.com/art.jpg', 1)",
            [],
        )
        .unwrap();
        let sub_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, duration_ms)
             VALUES (?1, 'g1', 'Episode One', 'https://example.com/e1.mp3', '2026-01-01T00:00:00Z', 3600000)",
            [sub_id],
        )
        .unwrap();
        let episode_id = conn.last_insert_rowid();

        let info = episode_display_info(&conn, episode_id).unwrap().unwrap();
        assert_eq!(info.title, "Episode One");
        assert_eq!(info.podcast_title, "Great Show");
        assert_eq!(info.artwork_url.as_deref(), Some("https://example.com/art.jpg"));
        assert_eq!(info.duration_ms, Some(3600000));

        assert!(episode_display_info(&conn, 9999).unwrap().is_none());
    }

    #[test]
    fn list_queue_orders_by_group_priority_then_position() {
        use crate::db::{migrate, open_connection};

        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        migrate(&conn).unwrap();

        // Default (sort_order 1.0, seeded by schema.sql) and a lower-priority
        // group.
        conn.execute("INSERT INTO groups (id, name, sort_order) VALUES (2, 'Later', 2.0)", [])
            .unwrap();

        conn.execute(
            "INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/a', 'Show A', 1)",
            [],
        )
        .unwrap();
        let sub_a = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/b', 'Show B', 2)",
            [],
        )
        .unwrap();
        let sub_b = conn.last_insert_rowid();

        // Group 2's episode is inserted first, so the order must come from
        // group priority, with explicit positions inside group 1.
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, group_queue_position)
             VALUES (?1, 'b1', 'B Episode', 'https://example.com/b1.mp3', '2026-01-01T00:00:00Z', 1.0)",
            [sub_b],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, group_queue_position)
             VALUES (?1, 'a2', 'A Episode Two', 'https://example.com/a2.mp3', '2026-01-01T00:00:00Z', 2.0)",
            [sub_a],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, group_queue_position)
             VALUES (?1, 'a1', 'A Episode One', 'https://example.com/a1.mp3', '2026-01-01T00:00:00Z', 1.0)",
            [sub_a],
        )
        .unwrap();
        // Not queued.
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date)
             VALUES (?1, 'a3', 'A Episode Three (not queued)', 'https://example.com/a3.mp3', '2026-01-01T00:00:00Z')",
            [sub_a],
        )
        .unwrap();

        let queue = list_queue(&conn).unwrap();
        assert_eq!(queue.len(), 3);
        // Group 1 in position order, then group 2.
        assert_eq!(queue[0].title, "A Episode One");
        assert_eq!(queue[0].group_name, "Default");
        assert_eq!(queue[1].title, "A Episode Two");
        assert_eq!(queue[2].title, "B Episode");
        assert_eq!(queue[2].group_name, "Later");
    }

    fn setup_two_queued_episodes_same_group() -> (tempfile::TempDir, Connection, i64, i64) {
        use crate::db::{migrate, open_connection};
        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        migrate(&conn).unwrap();
        conn.execute("INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/a', 'Show A', 1)", [])
            .unwrap();
        let sub_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, group_queue_position)
             VALUES (?1, 'e1', 'Episode One', 'https://example.com/e1.mp3', '2026-01-01T00:00:00Z', 1.0)",
            [sub_id],
        )
        .unwrap();
        let ep1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, group_queue_position)
             VALUES (?1, 'e2', 'Episode Two', 'https://example.com/e2.mp3', '2026-01-01T00:00:00Z', 2.0)",
            [sub_id],
        )
        .unwrap();
        let ep2 = conn.last_insert_rowid();
        (dir, conn, ep1, ep2)
    }

    #[test]
    fn reorder_queued_episode_moves_dragged_after_target_within_same_group() {
        let (_dir, conn, ep1, ep2) = setup_two_queued_episodes_same_group();
        // Dragging ep1 after ep2 reverses them.
        reorder_queued_episode(&conn, ep1, ep2).unwrap();

        let queue = list_queue(&conn).unwrap();
        assert_eq!(queue[0].episode_id, ep2);
        assert_eq!(queue[1].episode_id, ep1);
    }

    #[test]
    fn reorder_queued_episode_refuses_across_different_groups() {
        use crate::db::{migrate, open_connection};
        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        migrate(&conn).unwrap();
        conn.execute("INSERT INTO groups (id, name, sort_order) VALUES (2, 'Later', 2.0)", []).unwrap();
        conn.execute("INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/a', 'Show A', 1)", [])
            .unwrap();
        let sub_a = conn.last_insert_rowid();
        conn.execute("INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/b', 'Show B', 2)", [])
            .unwrap();
        let sub_b = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, group_queue_position)
             VALUES (?1, 'a1', 'A Episode', 'https://example.com/a1.mp3', '2026-01-01T00:00:00Z', 1.0)",
            [sub_a],
        )
        .unwrap();
        let ep_a = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, group_queue_position)
             VALUES (?1, 'b1', 'B Episode', 'https://example.com/b1.mp3', '2026-01-01T00:00:00Z', 1.0)",
            [sub_b],
        )
        .unwrap();
        let ep_b = conn.last_insert_rowid();

        let result = reorder_queued_episode(&conn, ep_a, ep_b);
        assert!(matches!(result, Err(ReorderError::DifferentGroups)));

        // Positions are unchanged.
        let queue = list_queue(&conn).unwrap();
        assert_eq!(queue.iter().find(|e| e.episode_id == ep_a).unwrap().group_id, 1);
    }

    #[test]
    fn reorder_queued_episode_onto_itself_is_a_no_op() {
        let (_dir, conn, ep1, ep2) = setup_two_queued_episodes_same_group();
        reorder_queued_episode(&conn, ep1, ep1).unwrap();
        let queue = list_queue(&conn).unwrap();
        assert_eq!(queue[0].episode_id, ep1);
        assert_eq!(queue[1].episode_id, ep2);
    }

    #[test]
    fn move_episode_to_front_of_group_if_member_moves_when_actually_a_member() {
        let (_dir, conn, ep1, ep2) = setup_two_queued_episodes_same_group();
        // Dropped on its group header, ep2 goes to the front.
        move_episode_to_front_of_group_if_member(&conn, ep2, 1).unwrap();

        let queue = list_queue(&conn).unwrap();
        assert_eq!(queue[0].episode_id, ep2);
        assert_eq!(queue[1].episode_id, ep1);
    }

    #[test]
    fn move_episode_to_front_of_group_if_member_refuses_when_not_actually_a_member() {
        let (_dir, conn, ep1, _ep2) = setup_two_queued_episodes_same_group();
        // ep1 is in group 1, so claiming group 2 is refused.
        let result = move_episode_to_front_of_group_if_member(&conn, ep1, 2);
        assert!(matches!(result, Err(ReorderError::DifferentGroups)));
    }

    #[test]
    fn next_episode_excluding_skips_the_loaded_episode() {
        use crate::db::{migrate, open_connection};

        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        migrate(&conn).unwrap();
        conn.execute("INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/f', 'Show', 1)", [])
            .unwrap();
        let sub_id = conn.last_insert_rowid();
        let mut ids = Vec::new();
        for (guid, pos) in [("a", 1.0), ("b", 2.0)] {
            conn.execute(
                "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, local_file_path, group_queue_position)
                 VALUES (?1, ?2, ?2, 'https://example.com/x.mp3', '2026-01-01T00:00:00Z', '/tmp/x.mp3', ?3)",
                rusqlite::params![sub_id, guid, pos],
            )
            .unwrap();
            ids.push(conn.last_insert_rowid());
        }

        assert_eq!(next_episode(&conn).unwrap().unwrap().episode_id, ids[0]);
        assert_eq!(next_episode_excluding(&conn, &[ids[0]]).unwrap().unwrap().episode_id, ids[1]);
        dequeue_episode(&conn, ids[1]).unwrap();
        assert!(next_episode_excluding(&conn, &ids).unwrap().is_none());
        assert_eq!(next_episode_excluding(&conn, &[ids[1]]).unwrap().unwrap().episode_id, ids[0]);
    }

    /// Three queued episodes in one group plus one in a later group.
    fn setup_interruption() -> (tempfile::TempDir, Connection, [i64; 4]) {
        use crate::db::{migrate, open_connection};
        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        migrate(&conn).unwrap();
        conn.execute("INSERT INTO groups (id, name, sort_order) VALUES (2, 'Later', 2.0)", []).unwrap();
        conn.execute("INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/a', 'A', 1)", []).unwrap();
        let sub_a = conn.last_insert_rowid();
        conn.execute("INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/b', 'B', 2)", []).unwrap();
        let sub_b = conn.last_insert_rowid();
        let mut ids = [0; 4];
        for (i, (sub, pos)) in [(sub_a, 1.0), (sub_a, 2.0), (sub_a, 3.0), (sub_b, 1.0)].into_iter().enumerate() {
            conn.execute(
                "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, group_queue_position)
                 VALUES (?1, ?2, 'E', 'https://example.com/e.mp3', '2026-01-01T00:00:00Z', ?3)",
                rusqlite::params![sub, format!("g{i}"), pos],
            )
            .unwrap();
            ids[i] = conn.last_insert_rowid();
        }
        (dir, conn, ids)
    }

    fn order(conn: &Connection) -> Vec<i64> {
        list_queue(conn).unwrap().iter().map(|e| e.episode_id).collect()
    }

    fn send_to_back(conn: &Connection, episode: i64) {
        conn.execute("UPDATE episodes SET group_queue_position = 10.0 WHERE id = ?1", [episode]).unwrap();
    }

    fn listened(conn: &Connection, episode: i64, ms: i64) {
        conn.execute("UPDATE episodes SET playback_position_ms = ?1 WHERE id = ?2", [ms, episode]).unwrap();
    }

    #[test]
    fn an_episode_barely_started_keeps_the_users_order() {
        let (_dir, conn, [a, b, c, d]) = setup_interruption();
        // a was loaded with a moment of progress, then the user put it last.
        listened(&conn, a, 400);
        send_to_back(&conn, a);
        assert_eq!(order(&conn), [b, c, a, d]);
        place_after_interruption(&conn, a, b).unwrap();
        assert_eq!(order(&conn), [b, c, a, d]);
        // Playing one further down brings only that one to the front.
        place_after_interruption(&conn, b, c).unwrap();
        assert_eq!(order(&conn), [c, b, a, d]);
    }

    #[test]
    fn the_threshold_is_thirty_seconds() {
        let (_dir, conn, [a, b, c, d]) = setup_interruption();
        listened(&conn, a, STARTED_THRESHOLD_MS - 1);
        place_after_interruption(&conn, a, c).unwrap();
        assert_eq!(order(&conn), [c, a, b, d], "c moved to the front, a kept its place");
        send_to_back(&conn, a);
        listened(&conn, a, STARTED_THRESHOLD_MS);
        place_after_interruption(&conn, a, b).unwrap();
        assert_eq!(order(&conn), [b, a, c, d]);
    }

    #[test]
    fn playing_an_episode_dragged_above_the_loaded_one_keeps_that_order() {
        let (_dir, conn, [a, b, c, d]) = setup_interruption();
        listened(&conn, a, 60_000);
        // The user drags c to the top, above a (loaded), and plays it.
        move_episode_to_front_of_group(&conn, c).unwrap();
        place_after_interruption(&conn, a, c).unwrap();
        assert_eq!(order(&conn), [c, a, b, d]);
    }

    #[test]
    fn the_interrupted_episode_resumes_next_after_one_played_out_of_order() {
        let (_dir, conn, [a, b, c, d]) = setup_interruption();
        listened(&conn, a, 60_000);
        place_after_interruption(&conn, a, c).unwrap();
        assert_eq!(order(&conn), [c, a, b, d]);
    }

    #[test]
    fn across_groups_the_interrupted_episode_goes_to_the_front_of_its_own() {
        let (_dir, conn, [a, b, c, d]) = setup_interruption();
        listened(&conn, c, 60_000);
        place_after_interruption(&conn, c, d).unwrap();
        assert_eq!(order(&conn), [c, a, b, d]);
    }

    #[test]
    fn unqueued_episodes_stay_unqueued() {
        let (_dir, conn, [a, b, c, d]) = setup_interruption();
        for e in [a, b, c] {
            listened(&conn, e, 60_000);
        }
        conn.execute("UPDATE episodes SET group_queue_position = NULL WHERE id = ?1", [b]).unwrap();
        // Playing an unqueued episode: the interrupted one goes to its group's front.
        place_after_interruption(&conn, c, b).unwrap();
        assert_eq!(order(&conn), [c, a, d]);
        // An unqueued interrupted episode isn't added to the queue.
        place_after_interruption(&conn, b, a).unwrap();
        assert_eq!(order(&conn), [a, c, d]);
    }
}
