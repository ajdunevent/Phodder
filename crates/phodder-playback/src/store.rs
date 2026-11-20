use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub(crate) struct PlayableEpisode {
    pub path: String,
    pub resume_ms: u64,
    pub info: crate::EpisodeInfo,
    pub feed_duration_ms: Option<u64>,
}

/// Played episodes resume from the start.
pub(crate) fn playable_episode(conn: &Connection, episode_id: i64) -> rusqlite::Result<Option<PlayableEpisode>> {
    let row: Option<(Option<String>, i64, bool)> = conn
        .query_row(
            "SELECT local_file_path, playback_position_ms, played FROM episodes WHERE id = ?1",
            [episode_id],
            |row| Ok((row.get(0)?, row.get::<_, Option<i64>>(1)?.unwrap_or(0), row.get::<_, Option<bool>>(2)?.unwrap_or(false))),
        )
        .optional()?;
    let Some((Some(path), position_ms, played)) = row else {
        return Ok(None);
    };
    let display = phodder_core::queue::episode_display_info(conn, episode_id)?;
    let (info, feed_duration_ms) = match display {
        Some(d) => (
            crate::EpisodeInfo {
                id: episode_id,
                title: d.title,
                podcast_title: d.podcast_title,
                artwork_url: d.artwork_url,
                artwork_local_path: d.artwork_local_path,
            },
            d.duration_ms.and_then(|ms| u64::try_from(ms).ok()),
        ),
        None => (crate::EpisodeInfo { id: episode_id, ..Default::default() }, None),
    };
    let resume_ms = if played { 0 } else { u64::try_from(position_ms).unwrap_or(0) };
    Ok(Some(PlayableEpisode { path, resume_ms, info, feed_duration_ms }))
}

pub(crate) fn chapters(conn: &Connection, episode_id: i64) -> rusqlite::Result<Vec<crate::Chapter>> {
    phodder_core::chapters::list(conn, episode_id)
}

/// The episode's podcast's chapter-skip rules, compiled. A malformed rule
/// set skips nothing rather than failing playback.
pub(crate) fn chapter_skip_rules(conn: &Connection, episode_id: i64) -> Vec<phodder_core::filter::CompiledFilterRule> {
    let rules = phodder_core::subscriptions::chapter_skip_rules_for_episode(conn, episode_id).unwrap_or_default();
    phodder_core::filter::compile_rules(&rules).unwrap_or_default()
}

pub(crate) fn next_episode(conn: &Connection, exclude: &[i64]) -> rusqlite::Result<Option<i64>> {
    Ok(phodder_core::queue::next_episode_excluding(conn, exclude)?.map(|n| n.episode_id))
}

pub(crate) fn last_in_progress(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM episodes
         WHERE download_status = 'downloaded' AND played = 0 AND playback_position_ms > 0
           AND group_queue_position IS NOT NULL
         ORDER BY position_updated_at DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .optional()
}

/// The timestamp only moves when the position does. Sync compares it with
/// remote actions, and restoring, seeking back to the saved spot or quitting
/// used to refresh it, so newer "played" actions from other devices lost.
pub(crate) fn save_position(conn: &Connection, episode_id: i64, position_ms: u64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE episodes SET
             position_updated_at = CASE WHEN abs(playback_position_ms - ?1) < 1000 THEN position_updated_at ELSE ?2 END,
             playback_position_ms = ?1
         WHERE id = ?3",
        rusqlite::params![position_ms as i64, chrono::Utc::now().to_rfc3339(), episode_id],
    )?;
    Ok(())
}

/// Sets `played` and `played_locally` (History lists only the latter) and
/// dequeues. Returns true on the unplayed-to-played transition.
/// `played_order` (History's sort key) is assigned on that transition, or
/// when an episode marked played by sync has none yet.
pub(crate) fn mark_played(conn: &Connection, episode_id: i64) -> rusqlite::Result<bool> {
    let was_played: bool =
        conn.query_row("SELECT played FROM episodes WHERE id = ?1", [episode_id], |row| row.get(0)).optional()?.unwrap_or(false);
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
    let changed = !was_played;
    // Dequeued even when already played. Skipping this for replayed
    // episodes made a one-episode queue repeat forever.
    phodder_core::queue::dequeue_episode(conn, episode_id)?;
    Ok(changed)
}

/// Natural end of stream. Position is reset so a later replay starts at
/// the beginning.
pub(crate) fn mark_finished(conn: &Connection, episode_id: i64) -> rusqlite::Result<bool> {
    let changed = mark_played(conn, episode_id)?;
    save_position(conn, episode_id, 0)?;
    Ok(changed)
}

pub(crate) fn place_after_interruption(conn: &Connection, interrupted: i64, now_playing: i64) -> rusqlite::Result<()> {
    phodder_core::queue::place_after_interruption(conn, interrupted, now_playing)
}

pub(crate) fn skip_forward_ms(conn: &Connection) -> u64 {
    setting_ms(conn, "skip_forward_ms", 30_000)
}

pub(crate) fn skip_backward_ms(conn: &Connection) -> u64 {
    setting_ms(conn, "skip_backward_ms", 10_000)
}

fn setting_ms(conn: &Connection, key: &str, default: u64) -> u64 {
    phodder_core::settings::get_setting_i64(conn, key).and_then(|v| u64::try_from(v).ok()).unwrap_or(default)
}

/// 0 or unset disables the threshold; natural end of stream still marks played.
pub(crate) fn played_threshold_ms(conn: &Connection) -> Option<u64> {
    phodder_core::settings::get_setting_f64(conn, "default_played_threshold_seconds")
        .filter(|s| *s > 0.0)
        .map(|s| (s * 1000.0) as u64)
}

pub(crate) fn saved_rate(conn: &Connection) -> f64 {
    phodder_core::settings::get_setting_f64(conn, "playback_rate")
        .filter(|r| crate::RATE_RANGE.contains(r))
        .unwrap_or(1.0)
}

pub(crate) fn save_rate(conn: &Connection, rate: f64) -> rusqlite::Result<()> {
    phodder_core::settings::set_setting(conn, "playback_rate", &rate.to_string())
}
