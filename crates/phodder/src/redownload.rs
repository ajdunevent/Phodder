//! Re-downloads an episode whose file turned out missing or damaged when it
//! was played. Once per episode per run, so a file that still won't play
//! (or a feed serving a bad file) doesn't loop.

use crate::i18n::{gettext, gettext_f};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

/// Long enough for a large episode on a slow connection. A download that
/// finishes later is kept but doesn't start playing out of nowhere.
const PLAY_WHEN_READY_WITHIN: std::time::Duration = std::time::Duration::from_secs(15 * 60);

struct Context {
    db: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
    download_tx: tokio::sync::broadcast::Sender<i64>,
    ui_tx: async_channel::Sender<crate::ui::UiUpdate>,
    player: phodder_playback::Player,
}

static CONTEXT: OnceLock<Context> = OnceLock::new();
static TRIED: Mutex<Option<HashSet<i64>>> = Mutex::new(None);

pub(crate) fn init(
    db: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
    download_tx: tokio::sync::broadcast::Sender<i64>,
    ui_tx: async_channel::Sender<crate::ui::UiUpdate>,
    player: phodder_playback::Player,
) {
    let _ = CONTEXT.set(Context { db, download_tx, ui_tx, player });
}

/// Re-downloads requested here skip the feed's download rules: the
/// episode was already chosen, and a rule changed since would drop it.
pub(crate) fn is_pending(episode_id: i64) -> bool {
    TRIED.lock().unwrap().as_ref().is_some_and(|tried| tried.contains(&episode_id))
}

/// Starts a re-download after a failed play and tells the user. Returns
/// false when it doesn't apply (already tried this run, or nothing to
/// download from), so the caller reports the failure as usual.
/// `play_when_ready` is for a play the user asked for.
pub(crate) async fn after_play_failure(episode_id: i64, play_when_ready: bool) -> bool {
    let Some(ctx) = CONTEXT.get() else { return false };
    if !TRIED.lock().unwrap().get_or_insert_with(HashSet::new).insert(episode_id) {
        return false;
    }
    let title = match reset_for_download(&ctx.db, episode_id).await {
        Ok(Some(title)) => title,
        Ok(None) => return false,
        Err(e) => {
            tracing::error!("failed to queue episode {episode_id} for re-download: {e}");
            return false;
        }
    };
    tracing::info!("episode {episode_id}'s file was missing or damaged; re-downloading");

    let held_back = if !crate::feed_health::network_available() {
        Some(gettext_f("“{title}” is damaged and will download again when you're online.", &[("title", &title)]))
    } else if !crate::downloader::automatic_work_allowed(&ctx.db).await.1 {
        Some(gettext_f("“{title}” is damaged and will download again on an unmetered connection.", &[("title", &title)]))
    } else {
        None
    };
    if let Some(message) = held_back {
        // Stays queued for the next unmetered pass or the network's return.
        let _ = ctx.ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
        let _ = ctx.ui_tx.send(crate::ui::UiUpdate::Notice(message)).await;
        return true;
    }

    let message = if play_when_ready {
        gettext("The download was damaged. Downloading it again to play when ready.")
    } else {
        gettext_f("Skipped “{title}”, which is damaged, and downloading it again.", &[("title", &title)])
    };
    let _ = ctx.ui_tx.send(crate::ui::UiUpdate::Notice(message)).await;
    if play_when_ready {
        // Subscribed before the download starts so its completion isn't missed.
        let mut completed = ctx.download_tx.subscribe();
        let player = ctx.player.clone();
        let ui_tx = ctx.ui_tx.clone();
        tokio::spawn(async move {
            let finished = tokio::time::timeout(PLAY_WHEN_READY_WITHIN, async {
                loop {
                    match completed.recv().await {
                        Ok(id) if id == episode_id => return true,
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
                    }
                }
            })
            .await;
            // Something the user started meanwhile isn't interrupted.
            if finished == Ok(true) && player.status() != phodder_playback::Status::Playing {
                if let Err(e) = player.send_play_episode(episode_id).await {
                    tracing::error!("episode {episode_id} could not be played after re-downloading: {e:#}");
                    let _ = ui_tx
                        .send(crate::ui::UiUpdate::Error(crate::i18n::gettext_f(
                            "Couldn't play the episode: {error}",
                            &[("error", &format!("{e:#}"))],
                        )))
                        .await;
                }
            }
        });
    }
    let db = ctx.db.clone();
    let download_tx = ctx.download_tx.clone();
    let ui_tx = ctx.ui_tx.clone();
    tokio::spawn(async move {
        let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
        if let Err(e) = crate::downloader::download_queued_episodes(&db, &download_tx, &ui_tx).await {
            tracing::error!("re-download of episode {episode_id} failed to start: {e:#}");
        }
    });
    true
}

/// Deletes what's left of the file and marks the episode for download,
/// keeping its queue place and position. A completed download only joins
/// the queue if the episode was already in it. `None` when the episode has
/// nothing to download from.
async fn reset_for_download(db: &tokio::sync::Mutex<rusqlite::Connection>, episode_id: i64) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let conn = db.lock().await;
    let row: Option<(String, String, Option<String>, String)> = conn
        .query_row(
            "SELECT title, audio_url, local_file_path, download_status FROM episodes WHERE id = ?1",
            [episode_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((title, audio_url, path, status)) = row else { return Ok(None) };
    if audio_url.is_empty() || status == "queued" || status == "downloading" {
        return Ok(None);
    }
    if let Some(path) = path {
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("failed to delete damaged file {path}: {e}");
            }
        }
    }
    conn.execute(
        "UPDATE episodes SET download_status = 'queued', local_file_path = NULL,
             skip_auto_enqueue = (group_queue_position IS NULL)
         WHERE id = ?1",
        [episode_id],
    )?;
    Ok(Some(title))
}
