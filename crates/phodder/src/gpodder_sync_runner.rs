//! Subscription and episode-action sync passes for the enabled sync account.

use std::collections::HashSet;

use rusqlite::OptionalExtension;

use crate::gpodder_sync::GpodderAccount;
use phodder_core::gpodder_sync::EpisodeActionRecord;

/// Looks up the account's secret. `device_id` applies only to gpodder.net.
///
/// The secret is copied into `GpodderAccount`'s plain `String` fields,
/// outside `Zeroizing`'s protection, for the duration of the pass.
async fn resolve_gpodder_account(
    db_account: &phodder_core::sync_accounts::SyncAccount,
    device_id: &str,
) -> anyhow::Result<GpodderAccount> {
    let credential_ref =
        db_account.credential_ref.as_deref().ok_or_else(|| anyhow::anyhow!("sync account has no stored credential reference"))?;
    let secret = crate::gpodder_credentials::get_credential(credential_ref)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read credential: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("credential not found in keyring for this account"))?;

    match db_account.kind.as_str() {
        "gpodder_net" => Ok(GpodderAccount::GpodderNet {
            base_url: db_account.server_url.clone().unwrap_or_else(|| "https://gpodder.net".to_string()),
            username: db_account.login_name.clone(),
            password: secret.to_string(),
            device_id: device_id.to_string(),
        }),
        "nextcloud_gpodder" => Ok(GpodderAccount::Nextcloud {
            server: db_account.server_url.clone().ok_or_else(|| anyhow::anyhow!("Nextcloud sync account is missing its server URL"))?,
            login_name: db_account.login_name.clone(),
            app_password: secret.to_string(),
        }),
        // "goa_nextcloud" is a schema-supported kind nothing constructs yet.
        other => anyhow::bail!("no sync implementation yet for account kind {other:?}"),
    }
}

/// Runs the subscription pass, then the episode-action pass. Both always run;
/// the first error is returned so Sync Now can report it. A no-op without an
/// enabled account.
pub async fn sync_all(db_path: &std::path::Path, ui_tx: &async_channel::Sender<crate::ui::UiUpdate>) -> anyhow::Result<()> {
    let subscriptions_result = sync_subscriptions(db_path, ui_tx).await;
    if let Err(e) = &subscriptions_result {
        tracing::error!("subscription sync failed: {e}");
    }
    let episode_actions_result = sync_episode_actions(db_path, ui_tx).await;
    if let Err(e) = &episode_actions_result {
        tracing::error!("episode-action sync failed: {e}");
    }
    // Sent unconditionally; DataChanged rebuilds are coalesced and skip
    // unchanged tabs.
    let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
    subscriptions_result?;
    episode_actions_result?;
    Ok(())
}

/// Uploads local subscription changes, downloads remote ones, applies
/// them, and updates each row's sync bookkeeping. Uses the `active` and
/// `synced_with_account_id` columns on `subscriptions` (see schema.sql);
/// a separate snapshot table was replaced by these. A no-op without an
/// enabled account.
pub async fn sync_subscriptions(db_path: &std::path::Path, ui_tx: &async_channel::Sender<crate::ui::UiUpdate>) -> anyhow::Result<()> {
    let db_account = {
        let conn = phodder_core::open_connection(db_path)?;
        phodder_core::sync_accounts::get_enabled_sync_account(&conn)?
    };
    let Some(db_account) = db_account else {
        return Ok(());
    };

    let device_id = {
        let conn = phodder_core::open_connection(db_path)?;
        phodder_core::settings::get_or_create_gpodder_device_id(&conn)?
    };
    let account = resolve_gpodder_account(&db_account, &device_id).await?;

    // Local changes since the last sync, plus inactive rows to purge without
    // reporting: ones this account never confirmed while active.
    let (locally_added, locally_removed, active_by_feed_url) = {
        let conn = phodder_core::open_connection(db_path)?;
        let mut stmt = conn.prepare("SELECT feed_url, active, synced_with_account_id FROM subscriptions")?;
        let rows: Vec<(String, bool, Option<i64>)> =
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?.collect::<rusqlite::Result<_>>()?;

        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut purge_now = Vec::new();
        let mut active_map = HashSet::new();
        for (feed_url, active, synced_with_account_id) in rows {
            let already_confirmed = synced_with_account_id == Some(db_account.id);
            if active {
                active_map.insert(feed_url.clone());
                if !already_confirmed {
                    added.push(feed_url);
                }
            } else if already_confirmed {
                removed.push(feed_url);
            } else {
                purge_now.push(feed_url);
            }
        }
        if !purge_now.is_empty() {
            tracing::info!("gpodder subscription sync: purging {} tombstone(s) with nothing to report", purge_now.len());
            for feed_url in &purge_now {
                if let Some(id) = subscription_id_for_feed_url(&conn, feed_url)? {
                    phodder_core::subscriptions::delete_subscription(&conn, id)?;
                }
            }
        }
        (added, removed, active_map)
    };

    if !locally_added.is_empty() || !locally_removed.is_empty() {
        tracing::info!(
            "gpodder subscription sync: uploading {} local addition(s), {} local removal(s)",
            locally_added.len(),
            locally_removed.len()
        );
        account.upload_subscription_changes(&locally_added, &locally_removed).await?;

        // Marks exactly what was reported; local state may have changed since
        // the lists were built.
        let conn = phodder_core::open_connection(db_path)?;
        for feed_url in &locally_added {
            if let Some(id) = subscription_id_for_feed_url(&conn, feed_url)? {
                conn.execute("UPDATE subscriptions SET synced_with_account_id = ?2 WHERE id = ?1", rusqlite::params![id, db_account.id])?;
            }
        }
        for feed_url in &locally_removed {
            if let Some(id) = subscription_id_for_feed_url(&conn, feed_url)? {
                // The server confirmed the removal, so the tombstone row
                // goes.
                phodder_core::subscriptions::delete_subscription(&conn, id)?;
            }
        }
    }

    // gpodder's "since" cursor is a Unix timestamp stored as text. A missing
    // or unparseable value means a full download, so a corrupt cursor can't
    // block syncing.
    let since = db_account.sync_since_cursor_utc.as_deref().and_then(|s| s.parse::<i64>().ok());
    let remote_changes = account.get_subscription_changes(since).await?;

    for feed_url in &remote_changes.add {
        if active_by_feed_url.contains(feed_url) {
            continue;
        }
        tracing::info!("gpodder subscription sync: subscribing to remotely-added feed {feed_url}");
        match crate::downloader::fetch_feed_preview(feed_url).await {
            Ok(preview) => match crate::ui::add_subscription_with_preview(db_path, feed_url, &preview) {
                Ok(new_id) => {
                    // Already known to the server; not reported back as a
                    // local add.
                    let conn = phodder_core::open_connection(db_path)?;
                    conn.execute(
                        "UPDATE subscriptions SET synced_with_account_id = ?2 WHERE id = ?1",
                        rusqlite::params![new_id, db_account.id],
                    )?;
                }
                Err(e) => {
                    tracing::error!("failed to add remotely-synced subscription {feed_url}: {e}");
                }
            },
            Err(e) => {
                tracing::warn!("failed to fetch preview for remotely-synced feed {feed_url}: {e}");
            }
        }
    }

    for feed_url in &remote_changes.remove {
        if !active_by_feed_url.contains(feed_url) {
            continue;
        }
        let conn = phodder_core::open_connection(db_path)?;
        let Some(subscription_id) = subscription_id_for_feed_url(&conn, feed_url)? else { continue };
        tracing::info!("gpodder subscription sync: unsubscribing from remotely-removed feed {feed_url}");
        if let Ok(episode_ids) = phodder_core::subscriptions::episode_ids_for_subscription(&conn, subscription_id) {
            let _ = ui_tx.send(crate::ui::UiUpdate::EpisodesRemoved(episode_ids)).await;
        }
        // Hard delete: the removal came from the server, so there's nothing
        // to report back.
        if let Err(e) = crate::ui::delete_subscription_and_files_hard(db_path, subscription_id) {
            tracing::error!("failed to remove remotely-unsynced subscription {feed_url}: {e}");
        }
    }

    {
        let conn = phodder_core::open_connection(db_path)?;
        phodder_core::sync_accounts::update_sync_cursor(&conn, db_account.id, &remote_changes.timestamp.to_string(), &chrono::Utc::now().to_rfc3339())?;
    }

    tracing::info!("gpodder subscription sync completed for account {}", db_account.id);
    Ok(())
}

/// Pushes the `sync_outbox`, pulls remote episode actions since this
/// account's episode-action cursor, and applies play position and played
/// status (download status and deletion aren't synced).
///
/// The later timestamp wins: a remote action older than the episode's
/// `position_updated_at` is skipped. A no-op without an enabled account.
pub async fn sync_episode_actions(db_path: &std::path::Path, ui_tx: &async_channel::Sender<crate::ui::UiUpdate>) -> anyhow::Result<()> {
    let db_account = {
        let conn = phodder_core::open_connection(db_path)?;
        phodder_core::sync_accounts::get_enabled_sync_account(&conn)?
    };
    let Some(db_account) = db_account else {
        return Ok(());
    };

    let device_id = {
        let conn = phodder_core::open_connection(db_path)?;
        phodder_core::settings::get_or_create_gpodder_device_id(&conn)?
    };
    let account = resolve_gpodder_account(&db_account, &device_id).await?;

    let pending = {
        let conn = phodder_core::open_connection(db_path)?;
        phodder_core::sync_accounts::pending_outbox_actions(&conn, db_account.id)?
    };
    if !pending.is_empty() {
        let mut records = Vec::with_capacity(pending.len());
        let mut record_ids = Vec::with_capacity(pending.len());
        {
            let conn = phodder_core::open_connection(db_path)?;
            for action in &pending {
                let Some((podcast, episode, guid, total_ms)) = episode_gpodder_identifiers(&conn, action.episode_id)? else {
                    // The episode was deleted locally; the entry is dropped.
                    record_ids.push(action.id);
                    continue;
                };
                records.push(EpisodeActionRecord {
                    podcast,
                    episode,
                    guid,
                    action: "play".to_string(),
                    timestamp: action.action_at_utc.clone(),
                    // Playback start isn't tracked separately; gpodder.net's
                    // examples always include `started` on "play" actions, so
                    // 0 is sent.
                    started: Some(0),
                    position: Some(action.position_ms.unwrap_or(0) / 1000),
                    total: total_ms.map(|ms| ms / 1000),
                    device: None,
                });
                record_ids.push(action.id);
            }
        }
        if !records.is_empty() {
            tracing::info!("gpodder episode-action sync: uploading {} pending action(s)", records.len());
            account.upload_episode_actions(&records).await?;
        }
        // Includes the dropped entries for deleted episodes.
        let conn = phodder_core::open_connection(db_path)?;
        phodder_core::sync_accounts::mark_outbox_actions_pushed(&conn, &record_ids)?;
    }

    // Separate cursor from sync_since_cursor_utc (see schema.sql).
    let since = db_account.episode_actions_sync_cursor_utc.as_deref().and_then(|s| s.parse::<i64>().ok());
    let remote_changes = account.get_episode_actions(since).await?;

    // Matches the local "played" rule, so an action stopped a second or two
    // short of the end (clients round) still counts.
    let played_slack_seconds = {
        let conn = phodder_core::open_connection(db_path)?;
        phodder_core::settings::get_setting_f64(&conn, "default_played_threshold_seconds").filter(|s| *s > 0.0).unwrap_or(0.0).max(2.0) as i64
    };
    let mut applied = false;
    let mut finished_episode_ids = Vec::new();
    for action in &remote_changes.actions {
        if action.action_kind() != Some(phodder_core::gpodder_sync::ActionKind::Play) {
            // Only "play" actions carry what's synced (position and played).
            continue;
        }
        let Some((episode_id, local_position_updated_at)) = find_local_episode(db_path, &action.podcast, &action.episode, action.guid.as_deref())?
        else {
            // Not subscribed or not downloaded here.
            continue;
        };

        if !remote_action_is_newer(&action.timestamp, local_position_updated_at.as_deref()) {
            // Local state is as current or newer.
            continue;
        }

        let Some(position_seconds) = action.position else { continue };
        let position_ms = position_seconds * 1000;
        // The protocol has no played flag; a position within the played
        // threshold of the duration means played.
        let played = action.total.is_some_and(|total| total > 0 && position_seconds + played_slack_seconds >= total);

        let conn = phodder_core::open_connection(db_path)?;
        conn.execute(
            "UPDATE episodes SET playback_position_ms = ?1, position_updated_at = ?2, played = ?3 WHERE id = ?4",
            rusqlite::params![position_ms, action.timestamp, played as i64, episode_id],
        )?;
        tracing::info!("gpodder episode-action sync: applied remote play action to episode {episode_id} (position={position_ms}ms, played={played})");
        applied = true;

        // Dequeued like a local completion. Manual downloads aren't exempt:
        // that flag protects the file from retention, not the queue entry.
        if played {
            if let Err(e) = phodder_core::queue::dequeue_episode(&conn, episode_id) {
                tracing::error!("episode_id={episode_id}: failed to dequeue after applying a remote played=true action: {e}");
            }
            finished_episode_ids.push(episode_id);
        }
    }
    if !finished_episode_ids.is_empty() {
        let _ = ui_tx.send(crate::ui::UiUpdate::EpisodesRemoved(finished_episode_ids)).await;
    }
    // Callers other than sync_all (playback-time syncs) send nothing else.
    if applied {
        let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
    }

    {
        let conn = phodder_core::open_connection(db_path)?;
        phodder_core::sync_accounts::update_episode_actions_sync_cursor(
            &conn,
            db_account.id,
            &remote_changes.timestamp.to_string(),
            &chrono::Utc::now().to_rfc3339(),
        )?;
    }

    tracing::info!("gpodder episode-action sync completed for account {}", db_account.id);
    Ok(())
}

/// Fields for an outgoing `EpisodeActionRecord`. `None` if the episode was
/// deleted locally.
fn episode_gpodder_identifiers(
    conn: &rusqlite::Connection,
    episode_id: i64,
) -> rusqlite::Result<Option<(String, String, Option<String>, Option<i64>)>> {
    conn.query_row(
        "SELECT s.feed_url, e.audio_url, e.guid, e.duration_ms
         FROM episodes e JOIN subscriptions s ON s.id = e.subscription_id
         WHERE e.id = ?1",
        rusqlite::params![episode_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, Option<String>>(2)?, row.get(3)?)),
    )
    .optional()
}

/// Matches by guid first, then feed URL plus media URL. Returns the episode
/// id and its `position_updated_at`.
fn find_local_episode(
    db_path: &std::path::Path,
    feed_url: &str,
    episode_url: &str,
    guid: Option<&str>,
) -> anyhow::Result<Option<(i64, Option<String>)>> {
    let conn = phodder_core::open_connection(db_path)?;
    if let Some(guid) = guid {
        let by_guid = conn
            .query_row(
                "SELECT e.id, e.position_updated_at FROM episodes e
                 JOIN subscriptions s ON s.id = e.subscription_id
                 WHERE s.feed_url = ?1 AND e.guid = ?2",
                rusqlite::params![feed_url, guid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if by_guid.is_some() {
            return Ok(by_guid);
        }
    }
    Ok(conn
        .query_row(
            "SELECT e.id, e.position_updated_at FROM episodes e
             JOIN subscriptions s ON s.id = e.subscription_id
             WHERE s.feed_url = ?1 AND e.audio_url = ?2",
            rusqlite::params![feed_url, episode_url],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// `true` when the local side has no timestamp yet.
fn remote_action_is_newer(remote_timestamp: &str, local_timestamp: Option<&str>) -> bool {
    let Some(local_timestamp) = local_timestamp else {
        return true;
    };
    let Ok(local) = chrono::DateTime::parse_from_rfc3339(local_timestamp) else {
        return false;
    };
    // Servers send offsets ("2026-09-14T21:57:42+00:00"), which the
    // documented naive format never matched, so every remote action was
    // ignored. RFC 3339 is tried first.
    let remote = chrono::DateTime::parse_from_rfc3339(remote_timestamp)
        .map(|dt| dt.to_utc())
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(remote_timestamp, "%Y-%m-%dT%H:%M:%S").map(|ndt| ndt.and_utc())
        });
    let Ok(remote) = remote else {
        // Unparseable timestamps never overwrite local state.
        return false;
    };
    remote > local.to_utc()
}

fn subscription_id_for_feed_url(conn: &rusqlite::Connection, feed_url: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row("SELECT id FROM subscriptions WHERE feed_url = ?1", rusqlite::params![feed_url], |row| row.get(0)).optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_action_is_newer_when_local_has_no_timestamp_yet() {
        assert!(remote_action_is_newer("2026-01-01T00:00:00", None));
    }

    #[test]
    fn remote_action_is_newer_when_genuinely_later() {
        assert!(remote_action_is_newer("2026-01-01T01:00:00", Some("2026-01-01T00:00:00+00:00")));
    }

    #[test]
    fn remote_action_is_not_newer_when_earlier() {
        // A late-syncing device's older action must not overwrite newer local
        // progress.
        assert!(!remote_action_is_newer("2026-01-01T00:00:00", Some("2026-01-01T01:00:00+00:00")));
    }

    #[test]
    fn remote_action_is_not_newer_when_equal() {
        assert!(!remote_action_is_newer("2026-01-01T00:00:00", Some("2026-01-01T00:00:00+00:00")));
    }

    #[test]
    fn remote_action_is_newer_compares_true_time_across_offsets() {
        // Local 04:30+05:00 is 23:30 UTC the previous day, earlier than the
        // remote's 00:00 UTC.
        assert!(remote_action_is_newer("2026-01-01T00:00:00", Some("2026-01-01T04:30:00+05:00")));
    }

    #[test]
    fn remote_action_is_not_newer_when_either_timestamp_fails_to_parse() {
        assert!(!remote_action_is_newer("not-a-real-timestamp", Some("2026-01-01T00:00:00+00:00")));
        assert!(!remote_action_is_newer("2026-01-01T00:00:00", Some("not-a-real-timestamp")));
    }

    #[test]
    fn remote_action_is_newer_handles_a_real_offset_suffixed_remote_timestamp() {
        // Captured from a real server response.
        assert!(remote_action_is_newer("2026-09-14T21:57:42+00:00", Some("2026-09-09T18:55:44+00:00")));
        assert!(!remote_action_is_newer("2026-09-09T18:55:44+00:00", Some("2026-09-14T21:57:42+00:00")));
    }
}
