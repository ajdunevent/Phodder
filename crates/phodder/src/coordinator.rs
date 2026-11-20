//! Owns the retention sweep and the playing state it must respect.

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::Duration;

/// Periodic sweep interval. Downloads, unloading an episode and marking one
/// played also sweep its subscription at once.
const RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 30);

pub struct Coordinator {
    pub conn: Arc<Mutex<rusqlite::Connection>>,
    db_path: std::path::PathBuf,
    currently_playing: Arc<Mutex<Option<i64>>>,
    /// Whether audio is playing, as opposed to `currently_playing`, which
    /// stays set while paused. The sweep syncs only when this is false.
    actively_playing: Arc<Mutex<bool>>,
}

impl Coordinator {
    pub fn new(conn: Arc<Mutex<rusqlite::Connection>>, db_path: std::path::PathBuf) -> Self {
        Self { conn, db_path, currently_playing: Arc::new(Mutex::new(None)), actively_playing: Arc::new(Mutex::new(false)) }
    }

    /// Called on every episode change; the sweep never evicts this episode.
    pub async fn set_now_playing(&self, episode_id: i64) {
        *self.currently_playing.lock().await = Some(episode_id);
    }

    /// Called when nothing is loaded (end of queue, Stop), not on pause: a
    /// paused episode can resume at any moment, so it stays protected.
    pub async fn clear_now_playing(&self) {
        *self.currently_playing.lock().await = None;
    }

    pub async fn set_actively_playing(&self, playing: bool) {
        *self.actively_playing.lock().await = playing;
    }

    pub fn spawn_retention_sweep(self: &Arc<Self>, ui_tx: async_channel::Sender<crate::ui::UiUpdate>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move { this.retention_sweep_loop(&ui_tx).await })
    }

    /// Sweeps a subscription as soon as one of its downloads completes, in
    /// addition to the periodic pass.
    pub fn spawn_download_triggered_sweep(
        self: &Arc<Self>,
        mut download_rx: tokio::sync::broadcast::Receiver<i64>,
    ) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                match download_rx.recv().await {
                    Ok(episode_id) => {
                        let playing = *this.currently_playing.lock().await;
                        sweep_episode_subscription(&this.conn, episode_id, playing).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        // Lagging only delays these sweeps to the next
                        // periodic pass.
                        tracing::warn!(
                            "download-triggered sweep lagged, skipped {skipped} event(s) (periodic sweep will still catch them)"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!("download-completion channel closed, download-triggered sweep stopping");
                        return;
                    }
                }
            }
        })
    }

    /// Runs `phodder_core::retention::evictable_episodes` for every
    /// subscription on a timer, deleting files and clearing their download
    /// state.
    async fn retention_sweep_loop(&self, ui_tx: &async_channel::Sender<crate::ui::UiUpdate>) {
        let mut interval = tokio::time::interval(RETENTION_SWEEP_INTERVAL);
        loop {
            interval.tick().await;
            tracing::info!("running periodic retention sweep");
            let playing = *self.currently_playing.lock().await;
            let subscription_ids = {
                let guard = self.conn.lock().await;
                match phodder_core::retention::list_subscription_ids(&guard) {
                    Ok(ids) => ids,
                    Err(e) => {
                        tracing::error!("failed to list subscriptions for retention sweep: {e}");
                        continue;
                    }
                }
            };
            for subscription_id in subscription_ids {
                self.sweep_subscription(subscription_id, playing).await;
            }

            // Syncs only while not playing; `PlayerCore::sync_while_playing`
            // covers playback. Spawned so a slow sync can't delay eviction.
            if !*self.actively_playing.lock().await {
                let db_path = self.db_path.clone();
                let ui_tx = ui_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::gpodder_sync_runner::sync_all(&db_path, &ui_tx).await {
                        tracing::error!("periodic sync (retention-sweep-triggered) failed: {e}");
                    }
                });
            }
        }
    }

    async fn sweep_subscription(&self, subscription_id: i64, currently_playing: Option<i64>) {
        sweep_subscription_retention(&self.conn, subscription_id, currently_playing).await;
    }

    /// Sweeps the subscription of an episode that was just unloaded, so a
    /// finished episode is deleted at once instead of at the next periodic
    /// pass, whose timer stops while the phone is suspended.
    pub async fn sweep_after_unload(&self, episode_id: i64) {
        let playing = *self.currently_playing.lock().await;
        sweep_episode_subscription(&self.conn, episode_id, playing).await;
    }
}

pub(crate) async fn sweep_episode_subscription(conn: &Arc<Mutex<rusqlite::Connection>>, episode_id: i64, currently_playing: Option<i64>) {
    let subscription_id = conn.lock().await.query_row(
        "SELECT subscription_id FROM episodes WHERE id = ?1",
        [episode_id],
        |row| row.get::<_, i64>(0),
    );
    match subscription_id {
        Ok(subscription_id) => sweep_subscription_retention(conn, subscription_id, currently_playing).await,
        Err(e) => tracing::error!("couldn't look up subscription for episode {episode_id}: {e}"),
    }
}

/// Standalone so `run_command_loop`, which has the database but no
/// `Coordinator`, can re-sweep one subscription right after its expiry
/// setting changes.
pub(crate) async fn sweep_subscription_retention(conn: &Arc<Mutex<rusqlite::Connection>>, subscription_id: i64, currently_playing: Option<i64>) {
    let guard = conn.lock().await;
    let evictable = match phodder_core::retention::evictable_episodes(&guard, subscription_id, currently_playing) {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!("retention sweep failed for subscription {subscription_id}: {e}");
            return;
        }
    };
    for episode_id in evictable {
        let path = match phodder_core::retention::episode_local_file_path(&guard, episode_id) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("failed to look up file path for episode {episode_id}: {e}");
                continue;
            }
        };
        if let Some(path) = path {
            match std::fs::remove_file(&path) {
                Ok(()) => tracing::info!("deleted {path} (episode {episode_id})"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!(
                        "episode {episode_id}'s file {path} was already gone -- clearing DB state anyway"
                    );
                }
                Err(e) => {
                    tracing::error!("failed to delete {path} for episode {episode_id}: {e}");
                    continue;
                }
            }
        }
        if let Err(e) = phodder_core::retention::mark_episode_deleted(&guard, episode_id) {
            tracing::error!("failed to mark episode {episode_id} deleted: {e}");
        }
    }
}
