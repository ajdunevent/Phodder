//! App-side adapter for the playback engine (`phodder-playback`). Turns
//! engine snapshots and events into UI updates, MPRIS signals, coordinator
//! state, and gpodder sync actions.

use crate::coordinator::Coordinator;
use phodder_playback::{Event, MilestoneKind, Player, Snapshot, Status};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use zbus::zvariant::Value;

const SYNC_WHILE_PLAYING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// Episode actions are pushed this long after the last playback
/// milestone, so a burst of pauses and seeks becomes one push.
const ACTION_PUSH_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

static SHUTDOWN_TARGET: OnceLock<(tokio::runtime::Handle, Player)> = OnceLock::new();

pub struct PlayerCore {
    pub player: Player,
    pub db: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
    pub db_path: std::path::PathBuf,
    pub ui_tx: async_channel::Sender<crate::ui::UiUpdate>,
    /// The connection MPRIS is registered on; signals are emitted on it.
    pub connection: zbus::Connection,
    action_push: tokio::sync::mpsc::UnboundedSender<()>,
    action_push_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<()>>>,
}

impl PlayerCore {
    pub fn new(
        player: Player,
        db: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
        db_path: std::path::PathBuf,
        ui_tx: async_channel::Sender<crate::ui::UiUpdate>,
        connection: zbus::Connection,
    ) -> Arc<Self> {
        let _ = SHUTDOWN_TARGET.set((tokio::runtime::Handle::current(), player.clone()));
        let (action_push, action_push_rx) = tokio::sync::mpsc::unbounded_channel();
        Arc::new(Self { player, db, db_path, ui_tx, connection, action_push, action_push_rx: std::sync::Mutex::new(Some(action_push_rx)) })
    }

    pub fn status(&self) -> Status {
        self.player.status()
    }

    pub fn episode_id(&self) -> Option<i64> {
        self.player.episode_id()
    }

    pub fn position_us(&self) -> i64 {
        self.player.snapshot().position_now_ms() as i64 * 1000
    }

    /// Spawns the tasks that forward engine state to the UI, MPRIS, the
    /// coordinator, and sync.
    pub fn spawn_adapters(self: &Arc<Self>, coordinator: Arc<Coordinator>) {
        let this = self.clone();
        tokio::spawn(async move { this.forward_snapshots(coordinator).await });
        let this = self.clone();
        tokio::spawn(async move { this.forward_events().await });
        let this = self.clone();
        tokio::spawn(async move { this.sync_while_playing().await });
        if let Some(rx) = self.action_push_rx.lock().unwrap().take() {
            let this = self.clone();
            tokio::spawn(async move { this.push_actions_debounced(rx).await });
        }
    }

    async fn forward_snapshots(&self, coordinator: Arc<Coordinator>) {
        let mut rx = self.player.watch();
        let mut previous: Option<Snapshot> = None;
        loop {
            let snap = rx.borrow_and_update().clone();
            let status_changed = previous.as_ref().is_none_or(|p| p.status != snap.status);
            let episode_changed = previous.as_ref().is_none_or(|p| p.episode != snap.episode || p.duration_ms != snap.duration_ms);

            if episode_changed {
                match &snap.episode {
                    Some(e) => coordinator.set_now_playing(e.id).await,
                    None => coordinator.clear_now_playing().await,
                }
                let unloaded = previous.as_ref().and_then(|p| p.episode.as_ref()).map(|e| e.id);
                if let Some(id) = unloaded.filter(|id| snap.episode.as_ref().is_none_or(|e| e.id != *id)) {
                    let coordinator = coordinator.clone();
                    tokio::spawn(async move { coordinator.sweep_after_unload(id).await });
                }
            }
            if status_changed {
                coordinator.set_actively_playing(snap.status == Status::Playing).await;
            }
            if status_changed || episode_changed {
                let mut changed = vec![("PlaybackStatus", Value::from(snap.status.as_mpris_str()))];
                if episode_changed {
                    changed.push(("Metadata", Value::from(mpris_metadata(&snap))));
                }
                self.emit_properties_changed(changed).await;
            }
            self.send_ui_state(&snap).await;

            previous = Some(snap);
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    async fn forward_events(&self) {
        let mut events = self.player.subscribe();
        loop {
            let event = match events.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("playback event forwarder lagged by {n} events");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            match event {
                Event::Milestone { episode_id, position_ms, kind } => {
                    tracing::debug!("playback milestone {kind:?} for episode {episode_id} at {position_ms}ms");
                    self.record_play_action(episode_id, position_ms as i64);
                    let _ = self.action_push.send(());
                    if kind == MilestoneKind::Finished {
                        let _ = self.ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
                    }
                }
                Event::Seeked { position_ms } => self.emit_seeked(position_ms as i64 * 1000).await,
                Event::LibraryChanged => {
                    let _ = self.ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
                }
                Event::EpisodeFailed { episode_id, message, file_problem, requested } => {
                    tracing::error!("episode {episode_id} could not be played: {message}");
                    if file_problem && crate::redownload::after_play_failure(episode_id, requested).await {
                        continue;
                    }
                    let error = crate::i18n::gettext_f("Couldn't play an episode, so it was skipped: {error}", &[("error", &message)]);
                    let _ = self.ui_tx.send(crate::ui::UiUpdate::Error(error)).await;
                }
            }
        }
    }

    async fn sync_while_playing(&self) {
        let mut interval = tokio::time::interval(SYNC_WHILE_PLAYING_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            if self.status() == Status::Playing {
                if let Err(e) = crate::gpodder_sync_runner::sync_episode_actions(&self.db_path, &self.ui_tx).await {
                    tracing::error!("periodic while-playing episode-action sync failed: {e}");
                }
            }
        }
    }

    async fn send_ui_state(&self, snap: &Snapshot) {
        let episode = snap.episode.clone().unwrap_or_default();
        let update = crate::ui::UiUpdate::PlaybackState {
            status: snap.status.as_mpris_str().to_string(),
            title: episode.title,
            podcast_title: episode.podcast_title,
            artwork_local_path: episode.artwork_local_path,
            position_us: snap.position_ms as i64 * 1000,
            duration_us: snap.duration_ms.map(|d| d as i64 * 1000),
            playback_rate: snap.rate,
            sleep_timer_remaining_secs: snap.sleep_remaining().map(|d| d.as_secs()),
            sleep_at_episode_end: snap.sleep_at_episode_end,
            episode_id: snap.episode.as_ref().map(|e| e.id),
            has_chapters: !snap.chapters.is_empty(),
            chapter_starts_ms: std::sync::Arc::new(snap.chapters.iter().map(|c| c.start_ms).collect()),
            chapter_title: snap.current_chapter().map(|c| c.title.clone()).filter(|t| !t.is_empty()),
        };
        if let Err(e) = self.ui_tx.send(update).await {
            tracing::debug!("no UI listening for playback state: {e}");
        }
    }

    async fn push_actions_debounced(&self, mut rx: tokio::sync::mpsc::UnboundedReceiver<()>) {
        while rx.recv().await.is_some() {
            // Each further milestone restarts the wait.
            loop {
                match tokio::time::timeout(ACTION_PUSH_DELAY, rx.recv()).await {
                    Ok(Some(())) => continue,
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            if !crate::feed_health::network_available() {
                continue;
            }
            if let Err(e) = crate::gpodder_sync_runner::sync_episode_actions(&self.db_path, &self.ui_tx).await {
                tracing::error!("episode-action push failed: {e}");
            }
        }
    }

    /// Queues a gpodder "play" action. No-op without an enabled sync account.
    pub(crate) fn record_play_action(&self, episode_id: i64, position_ms: i64) {
        let db_path = self.db_path.clone();
        tokio::spawn(async move {
            let result: anyhow::Result<()> = async {
                let conn = phodder_core::open_connection(&db_path)?;
                let Some(account) = phodder_core::sync_accounts::get_enabled_sync_account(&conn)? else {
                    return Ok(());
                };
                phodder_core::sync_accounts::enqueue_outbox_action(
                    &conn,
                    account.id,
                    episode_id,
                    "play",
                    Some(position_ms),
                    &chrono::Utc::now().to_rfc3339(),
                )?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                tracing::error!("failed to record play action for episode {episode_id}: {e}");
            }
        });
    }

    pub fn mpris_metadata_dict(&self) -> HashMap<String, Value<'static>> {
        mpris_metadata(&self.player.snapshot())
    }

    async fn emit_seeked(&self, position_us: i64) {
        if let Err(e) = self
            .connection
            .emit_signal(None::<()>, crate::mpris::MPRIS_OBJECT_PATH, "org.mpris.MediaPlayer2.Player", "Seeked", &(position_us,))
            .await
        {
            tracing::error!("failed to emit Seeked: {e}");
        }
    }

    async fn emit_properties_changed(&self, changed: Vec<(&str, Value<'_>)>) {
        let changed: HashMap<&str, Value<'_>> = changed.into_iter().collect();
        let invalidated: Vec<&str> = vec![];
        let body = ("org.mpris.MediaPlayer2.Player", changed, invalidated);
        if let Err(e) = self
            .connection
            .emit_signal(None::<()>, crate::mpris::MPRIS_OBJECT_PATH, "org.freedesktop.DBus.Properties", "PropertiesChanged", &body)
            .await
        {
            tracing::error!("failed to emit PropertiesChanged: {e}");
        }
    }
}

/// Saves position and releases the pipeline. Called on the GTK thread after
/// the application quits; without it, up to the 20s persist interval of
/// progress was lost.
pub fn shutdown_blocking() {
    let Some((handle, player)) = SHUTDOWN_TARGET.get() else { return };
    let result = handle.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(2), player.shutdown()).await });
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("playback shutdown failed: {e:#}"),
        Err(_) => tracing::warn!("playback shutdown timed out"),
    }
}

fn mpris_metadata(snap: &Snapshot) -> HashMap<String, Value<'static>> {
    let mut map = HashMap::new();
    let track_id = match &snap.episode {
        Some(e) => format!("/net/dunevent/Phodder/Episode/{}", e.id),
        None => "/org/mpris/MediaPlayer2/TrackList/NoTrack".to_string(),
    };
    let track_id = zbus::zvariant::ObjectPath::try_from(track_id)
        .unwrap_or_else(|_| zbus::zvariant::ObjectPath::from_static_str_unchecked("/org/mpris/MediaPlayer2/TrackList/NoTrack"));
    map.insert("mpris:trackid".to_string(), Value::from(track_id));
    if let Some(e) = &snap.episode {
        if !e.title.is_empty() {
            map.insert("xesam:title".to_string(), Value::from(e.title.clone()));
        }
        if !e.podcast_title.is_empty() {
            map.insert("xesam:artist".to_string(), Value::from(vec![e.podcast_title.clone()]));
            map.insert("xesam:album".to_string(), Value::from(e.podcast_title.clone()));
        }
        // Local art is preferred: lock screens can't always fetch remote URLs.
        let art = e
            .artwork_local_path
            .as_ref()
            .and_then(|p| phodder_playback::gstreamer::glib::filename_to_uri(p, None).ok().map(|u| u.to_string()))
            .or_else(|| e.artwork_url.clone());
        if let Some(art) = art {
            map.insert("mpris:artUrl".to_string(), Value::from(art));
        }
    }
    if let Some(d) = snap.duration_ms {
        map.insert("mpris:length".to_string(), Value::from(d as i64 * 1000));
    }
    map
}
