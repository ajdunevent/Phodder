//! Commands the UI sends to the service, carried over D-Bus as JSON, and
//! their handlers.

use crate::i18n::gettext_f;
use crate::player::PlayerCore;
use crate::ui::UiUpdate;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub enum ServiceCommand {
    PlayPause,
    RestartEpisode,
    PreviousChapter,
    NextChapter,
    NextEpisode,
    StopIfCurrent(Vec<i64>),
    PlayEpisode(i64),
    SetPlayed { episode_id: i64, played: bool },
    EnqueueEpisode(i64),
    DequeueEpisode(i64),
    /// Microseconds.
    SeekTo(i64),
    SkipForward,
    SkipBackward,
    SetPlaybackRate(f64),
    /// Milliseconds; `None` cancels the timer.
    SetSleepTimer(Option<u64>),
    SleepAtEpisodeEnd,
    RefreshSubscription(i64),
    RefreshAll,
    QueueBackCatalogEpisode { subscription_id: i64, episode: phodder_core::feed::ParsedEpisode, skip_enqueue: bool },
    RequeueEpisode(i64),
    ManuallyDownloadEpisode { episode_id: i64, skip_enqueue: bool },
    CancelDownload(i64),
    SyncSubscriptionsNow,
    ReevaluateSubscriptionRetention(i64),
    ReevaluateQueuedEpisodesAgainstFilters(i64),
    NetworkChanged(bool),
    /// Ends the service whatever Run in Background says.
    Quit,
}

pub type Reply = tokio::sync::oneshot::Sender<Result<(), String>>;

fn send_reply(reply: Option<Reply>, result: Result<(), String>) {
    if let Some(reply) = reply {
        let _ = reply.send(result);
    }
}

/// Player commands go to the engine without spawning, so their order is
/// preserved. Everything else runs as its own task, so a slow refresh can't
/// hold up the commands behind it.
pub fn handle(
    command: ServiceCommand,
    reply: Option<Reply>,
    core: &Arc<PlayerCore>,
    download_tx: &tokio::sync::broadcast::Sender<i64>,
    db_path: &std::path::Path,
) {
    use ServiceCommand as C;
    let core = core.clone();
    let download_tx = download_tx.clone();
    match command {
        C::PlayPause => core.player.send_toggle_play(),
        C::StopIfCurrent(episode_ids) => core.player.send_stop_if_current(episode_ids),
        C::RestartEpisode => core.player.send_restart(),
        C::PreviousChapter => core.player.send_previous_chapter(),
        C::NextChapter => core.player.send_next_chapter(),
        C::NextEpisode => core.player.send_next(),
        C::SeekTo(position_us) => core.player.send_seek_to(position_us.max(0) as u64 / 1000),
        C::SkipForward => core.player.send_skip_forward(),
        C::SkipBackward => core.player.send_skip_backward(),
        C::SetPlaybackRate(rate) => core.player.send_set_rate(rate),
        C::SetSleepTimer(millis) => core.player.send_set_sleep_timer(millis.map(std::time::Duration::from_millis)),
        C::SleepAtEpisodeEnd => core.player.send_sleep_at_episode_end(),
        C::CancelDownload(episode_id) => crate::downloader::cancel_download(episode_id),
        C::PlayEpisode(episode_id) => {
            let result = core.player.send_play_episode(episode_id);
            tokio::spawn(async move {
                if let Err(e) = result.await {
                    tracing::error!("episode {episode_id} could not be played: {e:#}");
                    if phodder_playback::is_file_missing(&e) && crate::redownload::after_play_failure(episode_id, true).await {
                        return;
                    }
                    let _ = core.ui_tx.send(UiUpdate::Error(gettext_f("Couldn't play the episode: {error}", &[("error", &format!("{e:#}"))]))).await;
                }
            });
        }
        C::NetworkChanged(available) => {
            tokio::spawn(async move {
                let came_back = crate::feed_health::set_network_available(available);
                tracing::info!("network available: {available}");
                crate::feed_health::send_status(&core.ui_tx).await;
                if came_back {
                    crate::downloader::run_automatic_pass(&core.db, &download_tx, &core.ui_tx).await;
                }
            });
        }
        // Handled by the service's command loop.
        C::Quit => {}
        C::SetPlayed { episode_id, played } => {
            if played {
                core.player.send_stop_if_current(vec![episode_id]);
            }
            tokio::spawn(async move {
                let result = phodder_core::queue::set_played(&*core.db.lock().await, episode_id, played);
                match result {
                    Ok(()) => {
                        if played {
                            crate::coordinator::sweep_episode_subscription(&core.db, episode_id, core.episode_id()).await;
                        }
                        let _ = core.ui_tx.send(UiUpdate::DataChanged).await;
                    }
                    Err(e) => tracing::error!("failed to set played={played} on episode {episode_id}: {e}"),
                }
            });
        }
        C::EnqueueEpisode(episode_id) => {
            tokio::spawn(async move {
                let result = phodder_core::queue::enqueue_episode(&*core.db.lock().await, episode_id);
                match result {
                    Ok(()) => {
                        let _ = core.ui_tx.send(UiUpdate::DataChanged).await;
                    }
                    Err(e) => tracing::error!("failed to enqueue episode {episode_id}: {e}"),
                }
            });
        }
        C::DequeueEpisode(episode_id) => {
            core.player.send_stop_if_current(vec![episode_id]);
            tokio::spawn(async move {
                let result = phodder_core::queue::dequeue_episode(&*core.db.lock().await, episode_id);
                match result {
                    Ok(()) => {
                        let _ = core.ui_tx.send(UiUpdate::DataChanged).await;
                    }
                    Err(e) => tracing::error!("failed to dequeue episode {episode_id}: {e}"),
                }
            });
        }
        C::RefreshSubscription(subscription_id) => {
            tokio::spawn(async move {
                let sub = phodder_core::subscriptions::get_subscription(&*core.db.lock().await, subscription_id);
                match sub {
                    Ok(Some(sub)) => {
                        if let Err(e) = crate::downloader::refresh_subscription(&core.db, &sub, &core.ui_tx, false, true).await {
                            tracing::error!("failed to refresh subscription {subscription_id}: {e}");
                            crate::feed_health::record_failure(sub.id, &sub.title, &e, true);
                            crate::feed_health::send_status(&core.ui_tx).await;
                        } else if let Err(e) = crate::downloader::download_queued_episodes(&core.db, &download_tx, &core.ui_tx).await {
                            // refresh_subscription only marks the backfill
                            // 'queued'; without this pass a new
                            // subscription's episodes waited for the hourly one.
                            tracing::error!("download pass failed after refreshing newly-added subscription {subscription_id}: {e}");
                            let _ = core.ui_tx.send(UiUpdate::Error(gettext_f("Download pass failed: {error}", &[("error", &e.to_string())]))).await;
                        }
                    }
                    Ok(None) => tracing::warn!("RefreshSubscription command for {subscription_id}, but it no longer exists"),
                    Err(e) => tracing::error!("failed to look up subscription {subscription_id} for manual refresh: {e}"),
                }
            });
        }
        C::RefreshAll => {
            tokio::spawn(async move {
                crate::downloader::refresh_and_download_now(&core.db, &download_tx, &core.ui_tx, true).await;
            });
        }
        C::QueueBackCatalogEpisode { subscription_id, episode, skip_enqueue } => {
            tokio::spawn(async move {
                let result =
                    crate::downloader::queue_back_catalog_episode(&core.db, subscription_id, &episode, skip_enqueue, &download_tx, &core.ui_tx)
                        .await
                        .map_err(|e| e.to_string());
                send_reply(reply, result);
            });
        }
        C::RequeueEpisode(episode_id) => {
            tokio::spawn(async move {
                let result: anyhow::Result<()> = async {
                    core.db
                        .lock()
                        .await
                        .execute("UPDATE episodes SET download_status = 'queued' WHERE id = ?1", rusqlite::params![episode_id])?;
                    let _ = core.ui_tx.send(UiUpdate::DataChanged).await;
                    crate::downloader::download_queued_episodes(&core.db, &download_tx, &core.ui_tx).await
                }
                .await;
                send_reply(reply, result.map_err(|e| e.to_string()));
            });
        }
        C::ManuallyDownloadEpisode { episode_id, skip_enqueue } => {
            tokio::spawn(async move {
                let result: anyhow::Result<()> = async {
                    core.db.lock().await.execute(
                        "UPDATE episodes SET download_status = 'queued', manually_downloaded = 1, skip_auto_enqueue = ?2 WHERE id = ?1",
                        rusqlite::params![episode_id, skip_enqueue],
                    )?;
                    let _ = core.ui_tx.send(UiUpdate::DataChanged).await;
                    crate::downloader::download_single_episode_directly(&core.db, &download_tx, &core.ui_tx, episode_id).await
                }
                .await;
                send_reply(reply, result.map_err(|e| e.to_string()));
            });
        }
        C::SyncSubscriptionsNow => {
            let db_path = db_path.to_path_buf();
            tokio::spawn(async move {
                let result = crate::gpodder_sync_runner::sync_all(&db_path, &core.ui_tx).await.map_err(|e| e.to_string());
                send_reply(reply, result);
            });
        }
        C::ReevaluateSubscriptionRetention(subscription_id) => {
            tokio::spawn(async move {
                crate::coordinator::sweep_subscription_retention(&core.db, subscription_id, core.episode_id()).await;
                let _ = core.ui_tx.send(UiUpdate::DataChanged).await;
            });
        }
        C::ReevaluateQueuedEpisodesAgainstFilters(subscription_id) => {
            tokio::spawn(async move {
                let any_removed = dequeue_newly_blocked(&*core.db.lock().await, subscription_id);
                if any_removed {
                    let _ = core.ui_tx.send(UiUpdate::DataChanged).await;
                }
            });
        }
    }
}

/// Dequeues the subscription's queued, non-manual episodes that its current
/// filter rules now block. True when any were removed.
fn dequeue_newly_blocked(conn: &rusqlite::Connection, subscription_id: i64) -> bool {
    let rules = match phodder_core::subscriptions::get_subscription(conn, subscription_id) {
        Ok(Some(sub)) => sub.download_filter_rules,
        Ok(None) => return false,
        Err(e) => {
            tracing::error!("failed to load subscription {subscription_id} for filter re-evaluation: {e}");
            return false;
        }
    };
    let compiled = match phodder_core::filter::compile_rules(&rules) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to compile filter rules for subscription {subscription_id}: {e}");
            return false;
        }
    };
    let candidates = match phodder_core::queue::queued_non_manual_episodes_for_subscription(conn, subscription_id) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to list queued episodes for subscription {subscription_id}: {e}");
            return false;
        }
    };
    let mut any_removed = false;
    for (episode_id, title, show_notes_html) in candidates {
        let show_notes_text = show_notes_html.as_deref().unwrap_or("");
        if phodder_core::filter::evaluate(&compiled, &title, show_notes_text) == phodder_core::filter::FilterDecision::Block {
            if let Err(e) = phodder_core::queue::dequeue_episode(conn, episode_id) {
                tracing::error!("failed to dequeue episode {episode_id} after a filter-rule change: {e}");
                continue;
            }
            tracing::info!("dequeued episode {episode_id}: now blocked by subscription {subscription_id}'s updated filter rules");
            any_removed = true;
        }
    }
    any_removed
}
