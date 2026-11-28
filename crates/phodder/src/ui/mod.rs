//! The UI layer: window construction and the message types that bridge it to
//! the backend running on its own Tokio runtime thread.

use gtk4::prelude::*;
use libadwaita::prelude::*;
use rusqlite::OptionalExtension;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use crate::service::commands::ServiceCommand;
#[allow(unused_imports)]
use crate::i18n::{gettext, gettext_f, ngettext, ngettext_f, N_};

mod window;
mod episode_detail;
mod problems;
mod add_feed;
mod discover;
mod queue;
mod library;
mod history;
mod subscriptions;
mod subscription_detail;
mod groups;
mod sync_settings;
mod settings;
mod widgets;
mod transcript;
mod selection;
mod backup;
pub(crate) mod client;

use window::*;
use episode_detail::*;
use add_feed::*;
use discover::*;
use queue::*;
use library::*;
use history::*;
use subscriptions::*;
use subscription_detail::*;
use groups::*;
use sync_settings::*;
use settings::*;
use widgets::*;
pub use window::build_window;
pub(crate) use add_feed::add_subscription_with_preview;
pub(crate) use subscription_detail::delete_subscription_and_files_hard;

/// Updates from the service, which sends them over D-Bus as JSON, and from
/// this process's own command handlers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum UiUpdate {
    PlaybackState {
        status: String, // as phodder_playback::Status::as_mpris_str()
        title: String,
        podcast_title: String,
        /// The episode's cached artwork, or its subscription's.
        artwork_local_path: Option<String>,
        // Microseconds, as MPRIS uses. `duration_us` is None until the
        // duration is known, briefly after loading; show it as unknown, not
        // as an error.
        position_us: i64,
        duration_us: Option<i64>,
        /// Keeps the speed button current.
        playback_rate: f64,
        /// Seconds left on the sleep timer, shown beside its button.
        sleep_timer_remaining_secs: Option<u64>,
        /// The sleep timer stops playback when this episode ends.
        sleep_at_episode_end: bool,
        /// None for a preview, which isn't in the database.
        episode_id: Option<i64>,
        /// Streaming an episode of a podcast before subscribing.
        preview: bool,
        has_chapters: bool,
        /// Chapter starts, for marks on the scrub bar.
        chapter_starts_ms: Arc<Vec<u64>>,
        chapter_title: Option<String>,
    },
    /// Re-query everything. Sent after refreshes and finished downloads; the
    /// tabs skip rebuilding when their data is unchanged.
    DataChanged,
    /// Per episode, at most once per percentage point (see download_episode),
    /// so Library updates one row instead of rebuilding.
    DownloadProgress { episode_id: i64, percent: u8 },
    /// Network availability and feeds whose refresh failures are worth
    /// showing.
    RefreshStatus { offline: bool, feed_problems: Vec<crate::feed_health::FeedProblem> },
    /// Whether any feed refresh is running; spins the Refresh button.
    Refreshing(bool),
    /// After each sync: what failed, or None once it works.
    SyncStatus(Option<crate::gpodder_sync_runner::SyncProblem>),
    /// Shown in the problems banner. While no window is open the service
    /// holds the most recent ones for the next window.
    Error(String),
    /// Something the user should know that isn't a problem; a toast.
    Notice(String),
    /// A refresh found episodes for a podcast with notifications on. Titles
    /// are newest first.
    NewEpisodes { subscription_id: i64, podcast_title: String, episode_titles: Vec<String> },
    /// Sync finished or removed these episodes; a loaded one is unloaded.
    EpisodesRemoved(Vec<i64>),
}

/// UI-to-backend commands. Not `Clone`: some carry a oneshot reply sender.
#[derive(Debug)]
pub enum UiCommand {
    PlayPause,
    RestartEpisode,
    PreviousChapter,
    NextChapter,
    NextEpisode,
    SetPlayed { episode_id: i64, played: bool },
    /// Stops an episode's download; see downloader::cancel_download.
    CancelDownload(i64),
    /// Plays a chosen episode now. The interrupted episode returns to the
    /// front of its group; see phodder_playback's `play_episode`.
    PlayEpisode(i64),
    /// Adds a downloaded episode to the playback queue.
    EnqueueEpisode(i64),
    /// Leaves the queue; the file is kept.
    DequeueEpisode(i64),
    /// Sent before the user deletes episodes' files or rows, so a loaded one
    /// gives way to the idle player instead of lingering.
    StopIfCurrent(Vec<i64>),
    /// Seeks to an absolute position, in microseconds. The scrub bar sends
    /// one per value change while dragging; the engine coalesces them.
    SeekTo(i64),
    /// Skip buttons; the amounts come from settings.
    SkipForward,
    SkipBackward,
    PlayPreview { url: String, title: String, podcast_title: String, artwork_local_path: Option<String> },
    /// The player's speed. A podcast with its own speed keeps it; for an
    /// episode of one that's playing, the change lasts for that episode.
    SetPlaybackRate(f64),
    /// A podcast's speed or trimming changed (Some), or the player's Trim
    /// Silence setting (None); the loaded episode picks it up.
    ApplyPodcastPlayback(Option<i64>),
    /// `None` cancels the timer.
    SetSleepTimer(Option<std::time::Duration>),
    SleepAtEpisodeEnd,
    /// Refreshes a just-added subscription now instead of on the hourly pass,
    /// ignoring metered gating since the user asked.
    RefreshSubscription(i64),
    /// The refresh button: refreshes everything and downloads new episodes,
    /// ignoring metered gating.
    RefreshAll,
    /// Fetches and parses a URL for Add Feed without touching the database,
    /// so errors show before a subscription exists. Replies on a oneshot, not
    /// UiUpdate.
    /// Searches the enabled directories, or lists their popular podcasts
    /// when `query` is None. Replies on a oneshot.
    Discover {
        query: Option<String>,
        reply_tx: tokio::sync::oneshot::Sender<Result<crate::discovery::DiscoveryResults, String>>,
    },
    /// Saves the user's Podcast Index key and secret (the secret in the
    /// keyring), or removes both when None. On the Tokio runtime: oo7 needs it.
    SetPodcastIndexCredentials {
        credentials: Option<(String, zeroize::Zeroizing<String>)>,
        reply_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// None when the episode has no transcript or it couldn't be read.
    LoadTranscript {
        episode_id: i64,
        reply_tx: tokio::sync::oneshot::Sender<Option<Arc<phodder_core::transcript::Transcript>>>,
    },
    /// Ok(false) when the Background portal refused it (Flatpak only).
    SetRunInBackground {
        enabled: bool,
        reply_tx: tokio::sync::oneshot::Sender<Result<bool, String>>,
    },
    /// Ok(false) when the Background portal refused it (Flatpak only).
    SetAutostart {
        enabled: bool,
        reply_tx: tokio::sync::oneshot::Sender<Result<bool, String>>,
    },
    FetchFeedPreview {
        url: String,
        /// From Add Podcast's directory, whose artwork is cached separately.
        directory: bool,
        reply_tx: tokio::sync::oneshot::Sender<Result<crate::downloader::FeedPreview, String>>,
    },
    /// The full episode list for the Episodes tab, without touching the
    /// database or fetching artwork. Replies on a oneshot.
    FetchFullEpisodeList {
        feed_url: String,
        reply_tx: tokio::sync::oneshot::Sender<Result<phodder_core::feed::ParsedFeed, String>>,
    },
    /// Inserts a back-catalog episode if needed (with its artwork, as a
    /// refresh would) or resets its status, then downloads it now.
    QueueBackCatalogEpisode {
        subscription_id: i64,
        episode: phodder_core::feed::ParsedEpisode,
        /// True for download-only, false for "+". Both set
        /// manually_downloaded; see skip_auto_enqueue in schema.sql.
        skip_enqueue: bool,
        reply_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Retries a failed Library download. Doesn't set manually_downloaded: a
    /// retry is still the automatic download it was.
    RequeueEpisode {
        episode_id: i64,
        reply_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// History's "+" and download-only: a manual download (sets
    /// manually_downloaded), kept separate from RequeueEpisode so a retry
    /// can't be mistaken for one.
    ManuallyDownloadEpisode {
        episode_id: i64,
        skip_enqueue: bool,
        reply_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Asks the backend to send `DataChanged`, for dialogs without access to
    /// the tab slots.
    NotifyDataChanged,
    /// One episode's artwork for the Episodes tab. One per command, each its
    /// own task, so rows update as each finishes instead of in clumps.
    FetchEpisodeArtwork {
        guid: String,
        artwork_url: String,
        reply_tx: tokio::sync::oneshot::Sender<Option<String>>,
    },
    FetchDiscoverArtwork {
        artwork_url: String,
        reply_tx: tokio::sync::oneshot::Sender<Option<String>>,
    },
    /// Imports OPML feeds (see phodder_core::opml) through Add Feed's
    /// fetch-then-insert path, without per-feed confirmation.
    /// Already-subscribed URLs are skipped.
    ImportOpml {
        path: std::path::PathBuf,
        reply_tx: tokio::sync::oneshot::Sender<Result<OpmlImportSummary, String>>,
    },
    ExportOpml {
        path: std::path::PathBuf,
        reply_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// A copy of the backup in the data directory, which the service deletes.
    RestoreBackup {
        path: String,
        reply_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Sync account commands run on the Tokio runtime: reqwest and oo7 need
    /// its reactor, and running them with `spawn_future_local` panicked
    /// ("there is no reactor running").
    StartNextcloudLogin {
        server_url: String,
        reply_tx: tokio::sync::oneshot::Sender<Result<crate::nextcloud_login::LoginFlowStart, String>>,
    },
    /// `Arc<AtomicBool>` so the polling future is `Send`.
    PollNextcloudLogin {
        flow: crate::nextcloud_login::LoginFlowStart,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        reply_tx: tokio::sync::oneshot::Sender<Result<crate::nextcloud_login::LoginFlowResult, crate::nextcloud_login::LoginFlowError>>,
    },
    /// Stores the credential and adds the sync_accounts row after Login Flow
    /// v2 completes.
    CompleteNextcloudConnection {
        login_result: crate::nextcloud_login::LoginFlowResult,
        reply_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Checks the login, registers this device and saves the account. The
    /// error is ready to show.
    ConnectGpodderNet {
        server: String,
        username: String,
        password: zeroize::Zeroizing<String>,
        reply_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Revokes the app password (best-effort; see revoke_app_password),
    /// deletes the stored credential, then removes the account row.
    DisconnectSyncAccount {
        account: phodder_core::sync_accounts::SyncAccount,
        reply_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// One full sync pass for the enabled account; a no-op without one.
    SyncSubscriptionsNow {
        reply_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Re-applies one subscription's retention after its settings change. The
    /// backend sends DataChanged when done.
    ReevaluateSubscriptionRetention {
        subscription_id: i64,
    },
    /// Re-checks one subscription's queued, non-manual episodes against its
    /// current filter rules, dequeuing newly blocked ones.
    ReevaluateQueuedEpisodesAgainstFilters {
        subscription_id: i64,
    },
}

/// Builds the channel pair for main.rs.
pub struct UiBridge {
    pub command_tx: UnboundedSender<UiCommand>,
    pub command_rx: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    pub update_tx: async_channel::Sender<UiUpdate>,
    pub update_rx: async_channel::Receiver<UiUpdate>,
}

pub fn create_bridge() -> UiBridge {
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    let (update_tx, update_rx) = async_channel::unbounded();
    UiBridge { command_tx, command_rx, update_tx, update_rx }
}

/// Runs commands that only matter while the window is open in this process,
/// and forwards the rest to the service.
pub async fn run_command_loop(
    mut command_rx: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    client: std::sync::Arc<client::ServiceClient>,
    ui_tx: async_channel::Sender<UiUpdate>,
    db_path: std::path::PathBuf,
) {
    while let Some(command) = command_rx.recv().await {
        let command = match command.route() {
            Routed::Service(command, reply) => {
                // Sent in order, so player commands keep theirs.
                client.send(command, reply).await;
                continue;
            }
            Routed::Local(command) => command,
        };
        // Each runs as its own task: a slow fetch must not hold up the rest.
        match command {
            UiCommand::Discover { query, reply_tx } => {
                let db_path = db_path.clone();
                tokio::spawn(async move {
                    let (sources, podcast_index_key) = phodder_core::open_connection(&db_path)
                        .map(|conn| (crate::discovery::enabled_sources(&conn), crate::discovery::podcast_index_key(&conn)))
                        .unwrap_or_default();
                    // A missing or unreadable secret fails only that source.
                    let podcast_index = match podcast_index_key {
                        Some(key) => match crate::gpodder_credentials::get_credential(crate::discovery::PODCAST_INDEX_SECRET_REF).await {
                            Ok(Some(secret)) => Some(crate::discovery::PodcastIndexCredentials { key, secret }),
                            Ok(None) => None,
                            Err(e) => {
                                tracing::warn!("couldn't read the Podcast Index secret: {e}");
                                None
                            }
                        },
                        None => None,
                    };
                    let result = if sources.is_empty() {
                        Ok(crate::discovery::DiscoveryResults::default())
                    } else {
                        match &query {
                            Some(term) => crate::discovery::search(&sources, podcast_index.as_ref(), term).await,
                            None => crate::discovery::popular(&sources, podcast_index.as_ref()).await,
                        }
                    };
                    let _ = reply_tx.send(result.map_err(|e| format!("{e:#}")));
                });
            }
            UiCommand::LoadTranscript { episode_id, reply_tx } => {
                let db_path = db_path.clone();
                tokio::spawn(async move {
                    let _ = reply_tx.send(crate::transcripts::load(&db_path, episode_id).await);
                });
            }
            UiCommand::SetRunInBackground { enabled, reply_tx } => {
                let db_path = db_path.clone();
                tokio::spawn(async move {
                    let result = async {
                        if enabled && crate::background_portal::is_flatpak() && !crate::background_portal::request_background(false).await? {
                            return Ok(false);
                        }
                        let conn = phodder_core::open_connection(&db_path)?;
                        phodder_core::settings::set_setting(&conn, crate::RUN_IN_BACKGROUND_SETTING, if enabled { "true" } else { "false" })?;
                        anyhow::Ok(true)
                    }
                    .await;
                    if let Err(e) = &result {
                        tracing::error!("failed to update Run in Background: {e:#}");
                    }
                    let _ = reply_tx.send(result.map_err(|e| e.to_string()));
                });
            }
            UiCommand::SetAutostart { enabled, reply_tx } => {
                tokio::spawn(async move {
                    let result = async {
                        if crate::background_portal::is_flatpak() && !crate::background_portal::request_background(enabled).await? {
                            return Ok(false);
                        }
                        crate::set_autostart_enabled(enabled)?;
                        anyhow::Ok(true)
                    }
                    .await;
                    if let Err(e) = &result {
                        tracing::error!("failed to update Launch on Startup: {e:#}");
                    }
                    let _ = reply_tx.send(result.map_err(|e| e.to_string()));
                });
            }
            UiCommand::SetPodcastIndexCredentials { credentials, reply_tx } => {
                let db_path = db_path.clone();
                tokio::spawn(async move {
                    let result = async {
                        let secret_ref = crate::discovery::PODCAST_INDEX_SECRET_REF;
                        match &credentials {
                            Some((_, secret)) => crate::gpodder_credentials::store_labeled_credential(secret_ref, "Phodder Podcast Index API Secret", secret).await?,
                            None => crate::gpodder_credentials::delete_credential(secret_ref).await?,
                        }
                        let key = credentials.as_ref().map(|(key, _)| key.trim().to_string()).unwrap_or_default();
                        phodder_core::open_connection(&db_path)
                            .and_then(|conn| phodder_core::settings::set_setting(&conn, crate::discovery::PODCAST_INDEX_KEY_SETTING, &key))
                            .map_err(|e| e.to_string())
                    }
                    .await;
                    let _ = reply_tx.send(result);
                });
            }
            UiCommand::FetchFeedPreview { url, directory, reply_tx } => {
                tokio::spawn(async move {
                    let result = if directory {
                        crate::downloader::fetch_directory_feed_preview(&url).await
                    } else {
                        crate::downloader::fetch_feed_preview(&url).await
                    };
                    let result = result.map_err(|e| e.to_string());
                    // Add Feed was closed before the fetch finished.
                    let _ = reply_tx.send(result);
                });
            }
            UiCommand::FetchFullEpisodeList { feed_url, reply_tx } => {
                tokio::spawn(async move {
                    let result = crate::downloader::fetch_full_episode_list(&feed_url).await.map_err(|e| e.to_string());
                    let _ = reply_tx.send(result);
                });
            }
            UiCommand::NotifyDataChanged => {
                let ui_tx = ui_tx.clone();
                tokio::spawn(async move {
                    let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
                });
            }
            UiCommand::FetchEpisodeArtwork { guid, artwork_url, reply_tx } => {
                tokio::spawn(async move {
                    let result = crate::downloader::fetch_episode_artwork(guid, artwork_url).await;
                    let _ = reply_tx.send(result);
                });
            }
            UiCommand::FetchDiscoverArtwork { artwork_url, reply_tx } => {
                tokio::spawn(async move {
                    let _ = reply_tx.send(crate::downloader::fetch_discover_artwork(artwork_url).await);
                });
            }
            UiCommand::ImportOpml { path, reply_tx } => {
                let db_path = db_path.clone();
                let ui_tx = ui_tx.clone();
                tokio::spawn(async move {
                    let result = import_opml(&db_path, &path).await.map_err(|e| e.to_string());
                    if result.is_ok() {
                        let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
                    }
                    let _ = reply_tx.send(result);
                });
            }
            UiCommand::ExportOpml { path, reply_tx } => {
                let db_path = db_path.clone();
                tokio::spawn(async move {
                    let result = export_opml(&db_path, &path).map_err(|e| e.to_string());
                    let _ = reply_tx.send(result);
                });
            }
            UiCommand::StartNextcloudLogin { server_url, reply_tx } => {
                tokio::spawn(async move {
                    let result = crate::nextcloud_login::start(&server_url).await.map_err(|e| e.to_string());
                    let _ = reply_tx.send(result);
                });
            }
            UiCommand::PollNextcloudLogin { flow, cancelled, reply_tx } => {
                tokio::spawn(async move {
                    let result = crate::nextcloud_login::poll_until_complete(&flow, &cancelled).await;
                    let _ = reply_tx.send(result);
                });
            }
            UiCommand::CompleteNextcloudConnection { login_result, reply_tx } => {
                let db_path = db_path.clone();
                tokio::spawn(async move {
                    let result = async {
                        let credential_ref = crate::gpodder_credentials::credential_ref_for(
                            "nextcloud_gpodder",
                            Some(&login_result.server),
                            &login_result.login_name,
                        );
                        crate::gpodder_credentials::store_credential(&credential_ref, &login_result.app_password)
                            .await
                            .map_err(|e| format!("couldn't securely store the credential: {e}"))?;
                        let conn = phodder_core::open_connection(&db_path).map_err(|e| e.to_string())?;
                        phodder_core::sync_accounts::insert_sync_account(
                            &conn,
                            "nextcloud_gpodder",
                            Some(&login_result.server),
                            &login_result.login_name,
                            Some(&credential_ref),
                        )
                        .map_err(|e| format!("couldn't save the account locally: {e}"))?;
                        Ok(())
                    }
                    .await;
                    let _ = reply_tx.send(result);
                });
            }
            UiCommand::ConnectGpodderNet { server, username, password, reply_tx } => {
                let db_path = db_path.clone();
                tokio::spawn(async move {
                    let result = async {
                        let device_id = phodder_core::open_connection(&db_path)
                            .and_then(|conn| phodder_core::settings::get_or_create_gpodder_device_id(&conn))
                            .map_err(|e| e.to_string())?;
                        match crate::gpodder_sync::connect_gpodder_net(&server, &username, &password, &device_id).await {
                            Ok(()) => {}
                            Err(crate::gpodder_sync::ConnectError::WrongCredentials) => {
                                return Err(gettext("Wrong username or password"));
                            }
                            Err(crate::gpodder_sync::ConnectError::Other(e)) => {
                                tracing::warn!("gpodder.net connect failed: {e:#}");
                                return Err(gettext_f("Couldn't reach {server}", &[("server", &server)]));
                            }
                        }
                        let credential_ref = crate::gpodder_credentials::credential_ref_for("gpodder_net", Some(&server), &username);
                        crate::gpodder_credentials::store_credential(&credential_ref, &password)
                            .await
                            .map_err(|e| gettext_f("Couldn't save the password to the keyring: {error}", &[("error", &e)]))?;
                        let conn = phodder_core::open_connection(&db_path).map_err(|e| e.to_string())?;
                        phodder_core::sync_accounts::insert_sync_account(&conn, "gpodder_net", Some(&server), &username, Some(&credential_ref))
                            .map_err(|e| e.to_string())?;
                        Ok(())
                    }
                    .await;
                    let _ = reply_tx.send(result);
                });
            }
            UiCommand::DisconnectSyncAccount { account, reply_tx } => {
                let db_path = db_path.clone();
                tokio::spawn(async move {
                    // Best-effort; the account is removed locally regardless.
                    if let Some(credential_ref) = &account.credential_ref {
                        // Only Nextcloud's sign-in creates a password that
                        // can be revoked; gpodder.net uses the account's own.
                        if let (Some(server), "nextcloud_gpodder") = (&account.server_url, account.kind.as_str()) {
                            if let Ok(Some(app_password)) = crate::gpodder_credentials::get_credential(credential_ref).await {
                                if let Err(e) = crate::nextcloud_login::revoke_app_password(server, &account.login_name, &app_password).await {
                                    tracing::warn!("failed to revoke Nextcloud app password server-side: {e}");
                                }
                            }
                        }
                        if let Err(e) = crate::gpodder_credentials::delete_credential(credential_ref).await {
                            tracing::warn!("failed to delete stored credential: {e}");
                        }
                    }
                    let result = phodder_core::open_connection(&db_path)
                        .and_then(|conn| phodder_core::sync_accounts::delete_sync_account(&conn, account.id))
                        .map_err(|e| e.to_string());
                    let _ = reply_tx.send(result);
                });
            }
            _ => unreachable!("route() sends every other command to the service"),
        }
    }
}

pub(crate) enum Routed {
    Service(ServiceCommand, Option<crate::service::commands::Reply>),
    Local(UiCommand),
}

impl UiCommand {
    pub(crate) fn route(self) -> Routed {
        use ServiceCommand as S;
        let service = |command| Routed::Service(command, None);
        match self {
            UiCommand::PlayPause => service(S::PlayPause),
            UiCommand::RestartEpisode => service(S::RestartEpisode),
            UiCommand::PreviousChapter => service(S::PreviousChapter),
            UiCommand::NextChapter => service(S::NextChapter),
            UiCommand::NextEpisode => service(S::NextEpisode),
            UiCommand::StopIfCurrent(ids) => service(S::StopIfCurrent(ids)),
            UiCommand::PlayEpisode(id) => service(S::PlayEpisode(id)),
            UiCommand::SetPlayed { episode_id, played } => service(S::SetPlayed { episode_id, played }),
            UiCommand::EnqueueEpisode(id) => service(S::EnqueueEpisode(id)),
            UiCommand::DequeueEpisode(id) => service(S::DequeueEpisode(id)),
            UiCommand::SeekTo(position_us) => service(S::SeekTo(position_us)),
            UiCommand::SkipForward => service(S::SkipForward),
            UiCommand::SkipBackward => service(S::SkipBackward),
            UiCommand::SetPlaybackRate(rate) => service(S::SetPlaybackRate(rate)),
            UiCommand::ApplyPodcastPlayback(subscription_id) => service(S::ApplyPodcastPlayback(subscription_id)),
            UiCommand::PlayPreview { url, title, podcast_title, artwork_local_path } => {
                service(S::PlayPreview { url, title, podcast_title, artwork_local_path })
            }
            UiCommand::SetSleepTimer(duration) => service(S::SetSleepTimer(duration.map(|d| d.as_millis() as u64))),
            UiCommand::SleepAtEpisodeEnd => service(S::SleepAtEpisodeEnd),
            UiCommand::RefreshSubscription(id) => service(S::RefreshSubscription(id)),
            UiCommand::RefreshAll => service(S::RefreshAll),
            UiCommand::CancelDownload(id) => service(S::CancelDownload(id)),
            UiCommand::ReevaluateSubscriptionRetention { subscription_id } => service(S::ReevaluateSubscriptionRetention(subscription_id)),
            UiCommand::ReevaluateQueuedEpisodesAgainstFilters { subscription_id } => {
                service(S::ReevaluateQueuedEpisodesAgainstFilters(subscription_id))
            }
            UiCommand::QueueBackCatalogEpisode { subscription_id, episode, skip_enqueue, reply_tx } => {
                Routed::Service(S::QueueBackCatalogEpisode { subscription_id, episode, skip_enqueue }, Some(reply_tx))
            }
            UiCommand::RequeueEpisode { episode_id, reply_tx } => Routed::Service(S::RequeueEpisode(episode_id), Some(reply_tx)),
            UiCommand::RestoreBackup { path, reply_tx } => Routed::Service(S::RestoreBackup(path), Some(reply_tx)),
            UiCommand::ManuallyDownloadEpisode { episode_id, skip_enqueue, reply_tx } => {
                Routed::Service(S::ManuallyDownloadEpisode { episode_id, skip_enqueue }, Some(reply_tx))
            }
            UiCommand::SyncSubscriptionsNow { reply_tx } => Routed::Service(S::SyncSubscriptionsNow, Some(reply_tx)),
            other => Routed::Local(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_and_download_commands_go_to_the_service_and_fetches_stay_local() {
        assert!(matches!(UiCommand::SeekTo(5_000_000).route(), Routed::Service(ServiceCommand::SeekTo(5_000_000), None)));
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        assert!(matches!(
            UiCommand::RequeueEpisode { episode_id: 3, reply_tx }.route(),
            Routed::Service(ServiceCommand::RequeueEpisode(3), Some(_))
        ));
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        assert!(matches!(UiCommand::FetchFullEpisodeList { feed_url: "https://example.com/feed".into(), reply_tx }.route(), Routed::Local(_)));
    }

    #[test]
    fn commands_and_updates_survive_the_trip_over_d_bus() {
        let episode = phodder_core::feed::ParsedEpisode {
            guid: "g".into(),
            title: "Episode".into(),
            show_notes_html: None,
            audio_url: "https://example.com/a.mp3".into(),
            file_size_bytes: Some(1),
            duration_ms: Some(2),
            publish_date: chrono::Utc::now(),
            publish_date_estimated: false,
            chapters_url: None,
            transcript_url: None,
            artwork_url: None,
        };
        let command = ServiceCommand::QueueBackCatalogEpisode { subscription_id: 7, episode, skip_enqueue: true };
        let decoded: ServiceCommand = serde_json::from_str(&serde_json::to_string(&command).unwrap()).unwrap();
        assert!(matches!(decoded, ServiceCommand::QueueBackCatalogEpisode { subscription_id: 7, skip_enqueue: true, ref episode } if episode.guid == "g"));

        let update = UiUpdate::PlaybackState {
            status: "Playing".into(),
            title: "Episode".into(),
            podcast_title: "Podcast".into(),
            artwork_local_path: None,
            position_us: 1,
            duration_us: Some(2),
            playback_rate: 1.5,
            sleep_timer_remaining_secs: None,
            sleep_at_episode_end: false,
            episode_id: Some(9),
            preview: false,
            has_chapters: true,
            chapter_starts_ms: Arc::new(vec![0, 60_000]),
            chapter_title: Some("Intro".into()),
        };
        let decoded: UiUpdate = serde_json::from_str(&serde_json::to_string(&update).unwrap()).unwrap();
        assert!(matches!(decoded, UiUpdate::PlaybackState { episode_id: Some(9), ref chapter_starts_ms, .. } if **chapter_starts_ms == [0, 60_000]));
    }
}
