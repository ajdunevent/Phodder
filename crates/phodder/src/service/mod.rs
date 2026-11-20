//! The background service: playback, MPRIS, refreshes and downloads, sync,
//! retention, and the Bluetooth, call and network watchers. It never
//! initializes GTK, so running in the background costs no graphics memory.
//!
//! The UI is a separate process. It sends `ServiceCommand`s and receives
//! `UiUpdate`s over a private D-Bus interface, both as JSON, and reads the
//! database directly.

pub mod commands;

use crate::coordinator::Coordinator;
use crate::mpris::{MprisPlayer, MprisRoot, MPRIS_BUS_NAME, MPRIS_OBJECT_PATH};
use crate::player::PlayerCore;
use crate::ui::UiUpdate;
use commands::ServiceCommand;
use gtk4::gio::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;
use zbus::names::OwnedUniqueName;

pub const BUS_NAME: &str = "net.dunevent.Phodder.Service";
pub const OBJECT_PATH: &str = "/net/dunevent/Phodder/Service";
pub const INTERFACE: &str = "net.dunevent.Phodder.Service1";
pub const CALLS_INTERFACE: &str = "net.dunevent.Phodder.ServiceCalls1";

const UI_BUS_NAME: &str = crate::APP_ID;
const UI_OBJECT_PATH: &str = "/net/dunevent/Phodder";

/// How long an "unavailable" network reading at startup must last to count.
const OFFLINE_AT_START_GRACE_SECS: u32 = 5;

/// How long a `NextUpdates` call waits before returning empty.
const POLL_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
/// A window that hasn't asked for updates this long is forgotten.
const SILENT_WINDOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Updates kept for a window that is slow to collect them.
const MAX_QUEUED_UPDATES: usize = 500;

/// Errors kept while no window is open, shown when one next opens.
const MAX_HELD_ERRORS: usize = 20;

/// Work that must happen on the GLib main thread.
enum MainEvent {
    NewEpisodes { subscription_id: i64, podcast_title: String, episode_titles: Vec<String> },
    Quit,
}

pub fn run() -> anyhow::Result<()> {
    // NON_UNIQUE: the UI owns the app ID's bus name. This GApplication only
    // provides the main loop, the network monitor and notifications, which
    // use the app ID to find the icon and to open the UI when clicked.
    let app = gtk4::gio::Application::new(Some(crate::APP_ID), gtk4::gio::ApplicationFlags::NON_UNIQUE);
    let _hold = app.hold();
    app.connect_activate(|_| {});

    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel::<Incoming>();
    let (main_tx, main_rx) = async_channel::unbounded::<MainEvent>();
    let tokio_handle = spawn_runtime(command_rx, main_tx);

    // Only used where the notification server can't open the UI itself.
    let show_podcast = gtk4::gio::SimpleAction::new("show-podcast", Some(gtk4::glib::VariantTy::INT64));
    {
        let tokio_handle = tokio_handle.clone();
        show_podcast.connect_activate(move |_, parameter| {
            let subscription_id = parameter.and_then(|p| p.get::<i64>());
            tokio_handle.spawn(open_ui(subscription_id));
        });
    }
    app.add_action(&show_podcast);

    // Refreshes are skipped while offline and run again on reconnect.
    let monitor = gtk4::gio::NetworkMonitor::default();
    let send_network = move |available: bool| {
        let _ = command_tx.send(Incoming { command: ServiceCommand::NetworkChanged(available), reply: None });
    };
    if monitor.is_network_available() {
        send_network(true);
    } else {
        // Under Flatpak the portal monitor reads unavailable until its first
        // asynchronous reply, which showed the offline banner on nearly every
        // start. A false is reported only if it lasts.
        let monitor = monitor.clone();
        let send_network = send_network.clone();
        gtk4::glib::timeout_add_seconds_local_once(OFFLINE_AT_START_GRACE_SECS, move || {
            if !monitor.is_network_available() {
                send_network(false);
            }
        });
    }
    monitor.connect_network_changed(move |_, available| send_network(available));

    {
        let app = app.clone();
        gtk4::glib::spawn_future_local(async move {
            while let Ok(event) = main_rx.recv().await {
                match event {
                    MainEvent::NewEpisodes { subscription_id, podcast_title, episode_titles } => {
                        notify_new_episodes(&app, subscription_id, &podcast_title, &episode_titles)
                    }
                    MainEvent::Quit => app.quit(),
                }
            }
        });
    }

    app.run_with_args::<&str>(&[]);
    crate::player::shutdown_blocking();
    tracing::info!("phodder service exiting");
    Ok(())
}

fn notify_new_episodes(app: &gtk4::gio::Application, subscription_id: i64, podcast_title: &str, episode_titles: &[String]) {
    let body = match episode_titles {
        [only] => only.clone(),
        [newest, rest @ ..] => crate::i18n::ngettext_f(
            "{title} and {n} more",
            "{title} and {n} more",
            rest.len() as u32,
            &[("title", newest), ("n", &rest.len().to_string())],
        ),
        [] => return,
    };
    let notification = gtk4::gio::Notification::new(podcast_title);
    notification.set_body(Some(&body));
    notification.set_default_action_and_target_value("app.show-podcast", Some(&subscription_id.to_variant()));
    // One per podcast: a later refresh replaces it.
    app.send_notification(Some(&format!("new-episodes-{subscription_id}")), &notification);
}

/// Two workers: the backend mostly waits on the network and the disk, and
/// each extra thread can grow its own allocator arena.
fn spawn_runtime(command_rx: tokio::sync::mpsc::UnboundedReceiver<Incoming>, main_tx: async_channel::Sender<MainEvent>) -> tokio::runtime::Handle {
    let runtime = crate::build_runtime();
    let handle = runtime.handle().clone();
    std::thread::Builder::new()
        .name("service".into())
        .spawn(move || {
            runtime.block_on(async move {
                if let Err(e) = backend(command_rx, main_tx.clone()).await {
                    tracing::error!("phodder service failed: {e:#}");
                    let _ = main_tx.send(MainEvent::Quit).await;
                }
            })
        })
        .expect("couldn't start the service thread");
    handle
}

struct Incoming {
    command: ServiceCommand,
    reply: Option<commands::Reply>,
}

async fn backend(mut command_rx: tokio::sync::mpsc::UnboundedReceiver<Incoming>, main_tx: async_channel::Sender<MainEvent>) -> anyhow::Result<()> {
    let connection = zbus::Connection::session().await?;
    // Registered before the name is claimed: a window waiting for the name
    // calls the moment it appears, and zbus drops calls that arrive before an
    // object exists. Commands queue until the player is ready.
    let hub = Arc::new(Hub::default());
    let (iface_tx, mut iface_rx) = tokio::sync::mpsc::unbounded_channel::<Incoming>();
    connection.object_server().at(OBJECT_PATH, ServiceInterface { commands: iface_tx.clone(), hub: hub.clone(), main_tx: main_tx.clone() }).await?;
    connection.object_server().at(OBJECT_PATH, CallsInterface { commands: iface_tx, hub: hub.clone() }).await?;
    // Claimed before the player or downloads start, so a second service exits
    // without touching them.
    let claimed = connection.request_name_with_flags(BUS_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into()).await;
    // zbus reports an owned name as an error rather than as a reply.
    if matches!(claimed, Err(zbus::Error::NameTaken)) || matches!(claimed, Ok(ref reply) if *reply != zbus::fdo::RequestNameReply::PrimaryOwner) {
        tracing::info!("the Phodder service is already running");
        let _ = main_tx.send(MainEvent::Quit).await;
        return Ok(());
    }
    claimed?;

    let db_path = crate::db_path();
    let db = Arc::new(Mutex::new(phodder_core::open_connection(&db_path)?));
    let coordinator = Arc::new(Coordinator::new(db.clone(), db_path.clone()));

    // Downloads announce completed episodes so the coordinator can sweep at
    // once. A lagged receiver only defers those sweeps to the next periodic
    // pass.
    let (download_tx, download_rx) = tokio::sync::broadcast::channel::<i64>(16);
    coordinator.spawn_download_triggered_sweep(download_rx);

    let (ui_tx, ui_rx) = async_channel::unbounded::<UiUpdate>();
    {
        let db = db.clone();
        let download_tx = download_tx.clone();
        let ui_tx = ui_tx.clone();
        tokio::spawn(async move {
            // A panic resolves this task's JoinHandle with Err without taking
            // down the process.
            if let Err(e) = crate::downloader::run(db, download_tx, ui_tx).await {
                tracing::error!("downloader task exited: {e}");
            }
        });
    }

    let player = phodder_playback::Player::spawn(phodder_playback::Config { db: db.clone(), audio_sink: None })?;
    crate::redownload::init(db.clone(), download_tx.clone(), ui_tx.clone(), player.clone());
    let core = PlayerCore::new(player, db.clone(), db_path.clone(), ui_tx, connection.clone());
    core.spawn_adapters(coordinator.clone());

    connection.object_server().at(MPRIS_OBJECT_PATH, MprisRoot).await?;
    connection.object_server().at(MPRIS_OBJECT_PATH, MprisPlayer { core: core.clone() }).await?;
    connection.request_name(MPRIS_BUS_NAME).await?;

    if let Err(e) = core.player.restore_last_in_progress().await {
        tracing::error!("failed to restore last in-progress episode: {e:#}");
    }
    // Started after the restore so the sweep sees the restored episode as
    // currently playing.
    if let Some(id) = core.episode_id() {
        coordinator.set_now_playing(id).await;
    }
    coordinator.spawn_retention_sweep(core.ui_tx.clone());
    tokio::spawn(crate::downloader::prune_discover_artwork());
    tokio::spawn(crate::transcripts::prune(db_path.clone()));
    crate::call_watch::spawn_call_watcher(core.clone());
    crate::mpris_peer_watch::spawn_mpris_peer_watcher(core.clone());
    crate::bluetooth_watch::spawn_bluetooth_watcher(core.clone());

    tokio::spawn(forward_updates(ui_rx, hub.clone(), core.clone(), main_tx.clone()));
    tokio::spawn(watch_clients(connection.clone(), hub.clone(), main_tx.clone()));
    tokio::spawn(forget_silent_windows(hub.clone(), main_tx.clone()));

    tracing::info!("phodder service ready");

    loop {
        let incoming = tokio::select! {
            Some(incoming) = command_rx.recv() => incoming,
            Some(incoming) = iface_rx.recv() => incoming,
            else => break,
        };
        commands::handle(incoming.command, incoming.reply, &core, &download_tx, &db_path);
    }
    Ok(())
}

#[derive(Default)]
struct Last {
    playback: Option<UiUpdate>,
    refresh_status: Option<UiUpdate>,
}

/// Updates waiting for a window's next `NextUpdates` call.
struct Window {
    queue: VecDeque<String>,
    wake: Arc<tokio::sync::Notify>,
    last_seen: std::time::Instant,
}

/// The open windows (normally one) and what a newly opened one needs to
/// catch up on.
#[derive(Default)]
struct Hub {
    windows: Mutex<HashMap<OwnedUniqueName, Window>>,
    held_errors: Mutex<VecDeque<UiUpdate>>,
    last: Mutex<Last>,
}

impl Hub {
    async fn broadcast(&self, update: UiUpdate) {
        match &update {
            UiUpdate::PlaybackState { .. } => self.last.lock().await.playback = Some(update.clone()),
            UiUpdate::RefreshStatus { .. } => self.last.lock().await.refresh_status = Some(update.clone()),
            _ => {}
        }
        let mut windows = self.windows.lock().await;
        if windows.is_empty() {
            if matches!(update, UiUpdate::Error(_)) {
                let mut held = self.held_errors.lock().await;
                if held.len() == MAX_HELD_ERRORS {
                    held.pop_front();
                }
                held.push_back(update);
            }
            return;
        }
        let json = match serde_json::to_string(&update) {
            Ok(json) => json,
            Err(e) => {
                tracing::error!("couldn't encode an update: {e}");
                return;
            }
        };
        for window in windows.values_mut() {
            if window.queue.len() == MAX_QUEUED_UPDATES {
                window.queue.pop_front();
            }
            window.queue.push_back(json.clone());
            window.wake.notify_one();
        }
    }

    /// A newly seen window gets the player and feed state, and the errors
    /// that happened while none was open.
    async fn register(&self, name: &OwnedUniqueName) -> Arc<tokio::sync::Notify> {
        let mut windows = self.windows.lock().await;
        if let Some(window) = windows.get_mut(name) {
            window.last_seen = std::time::Instant::now();
            return window.wake.clone();
        }
        tracing::info!("a window connected");
        let last = self.last.lock().await;
        let held = self.held_errors.lock().await.drain(..).collect::<Vec<_>>();
        let queue = last
            .playback
            .iter()
            .chain(last.refresh_status.iter())
            .chain(held.iter())
            .filter_map(|update| serde_json::to_string(update).ok())
            .collect();
        let wake = Arc::new(tokio::sync::Notify::new());
        windows.insert(name.clone(), Window { queue, wake: wake.clone(), last_seen: std::time::Instant::now() });
        wake
    }

    /// Waits up to `POLL_WAIT` for updates. Empty when none came or the
    /// window was forgotten meanwhile.
    async fn next_updates(&self, name: &OwnedUniqueName) -> Vec<String> {
        let wake = self.register(name).await;
        loop {
            {
                let mut windows = self.windows.lock().await;
                let Some(window) = windows.get_mut(name) else { return Vec::new() };
                window.last_seen = std::time::Instant::now();
                if !window.queue.is_empty() {
                    return window.queue.drain(..).collect();
                }
            }
            if tokio::time::timeout(POLL_WAIT, wake.notified()).await.is_err() {
                return Vec::new();
            }
        }
    }

    /// True when this was the last window.
    async fn forget(&self, name: &str) -> bool {
        let mut windows = self.windows.lock().await;
        let before = windows.len();
        windows.retain(|n, _| n.as_str() != name);
        windows.len() < before && windows.is_empty()
    }
}

/// With Run in Background off, the service stops with the last window, as
/// closing the window always quit before.
async fn last_window_closed(main_tx: &async_channel::Sender<MainEvent>) {
    tracing::info!("the last window closed");
    if !crate::run_in_background() {
        let _ = main_tx.send(MainEvent::Quit).await;
    }
}

async fn forward_updates(ui_rx: async_channel::Receiver<UiUpdate>, hub: Arc<Hub>, core: Arc<PlayerCore>, main_tx: async_channel::Sender<MainEvent>) {
    while let Ok(update) = ui_rx.recv().await {
        match update {
            UiUpdate::NewEpisodes { subscription_id, podcast_title, episode_titles } => {
                let _ = main_tx.send(MainEvent::NewEpisodes { subscription_id, podcast_title, episode_titles }).await;
            }
            UiUpdate::EpisodesRemoved(episode_ids) => {
                // Sync finished or removed these; a loaded one gives way.
                core.player.send_stop_if_current(episode_ids);
            }
            update => hub.broadcast(update).await,
        }
    }
}

/// Forgets windows whose connection closes. Inside Flatpak this may not see
/// every window, so `Goodbye` and `forget_silent_windows` cover the rest.
async fn watch_clients(connection: zbus::Connection, hub: Arc<Hub>, main_tx: async_channel::Sender<MainEvent>) {
    let changes = async {
        let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
        dbus.receive_name_owner_changed().await
    }
    .await;
    let mut changes = match changes {
        Ok(changes) => changes,
        Err(e) => {
            tracing::warn!("couldn't watch for windows closing: {e}");
            return;
        }
    };
    use futures_util::StreamExt;
    while let Some(change) = changes.next().await {
        let Ok(args) = change.args() else { continue };
        if args.new_owner().is_some() {
            continue;
        }
        let zbus::names::BusName::Unique(name) = args.name() else { continue };
        if hub.forget(name.as_str()).await {
            last_window_closed(&main_tx).await;
        }
    }
}

/// A window that stops asking for updates has gone, as after a crash.
async fn forget_silent_windows(hub: Arc<Hub>, main_tx: async_channel::Sender<MainEvent>) {
    loop {
        tokio::time::sleep(POLL_WAIT).await;
        let mut windows = hub.windows.lock().await;
        let before = windows.len();
        windows.retain(|_, w| w.last_seen.elapsed() < SILENT_WINDOW_TIMEOUT);
        let none_left = windows.len() < before && windows.is_empty();
        drop(windows);
        if none_left {
            last_window_closed(&main_tx).await;
        }
    }
}

/// Opens the window, or a podcast's Episodes tab in it, starting the UI
/// through D-Bus activation when it isn't running.
pub async fn open_ui(subscription_id: Option<i64>) {
    let result = async {
        let connection = zbus::Connection::session().await?;
        let platform_data = std::collections::HashMap::<&str, zbus::zvariant::Value<'_>>::new();
        match subscription_id {
            Some(id) => {
                let parameter = vec![zbus::zvariant::Value::from(id)];
                connection
                    .call_method(Some(UI_BUS_NAME), UI_OBJECT_PATH, Some("org.freedesktop.Application"), "ActivateAction", &("show-podcast", parameter, platform_data))
                    .await?
            }
            None => connection.call_method(Some(UI_BUS_NAME), UI_OBJECT_PATH, Some("org.freedesktop.Application"), "Activate", &(platform_data,)).await?,
        };
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing::info!("D-Bus couldn't open the window ({e:#}); starting it directly");
        if let Err(e) = spawn_self(&[]) {
            tracing::error!("couldn't open the window: {e}");
        }
    }
}

/// Starts this binary as a separate process. A fallback for when D-Bus
/// activation isn't set up, as in a development build: inside Flatpak the
/// child would die with this process's sandbox.
pub fn spawn_self(args: &[&str]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(args)
        .stdin(std::process::Stdio::null())
        .process_group(0)
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

struct ServiceInterface {
    commands: tokio::sync::mpsc::UnboundedSender<Incoming>,
    hub: Arc<Hub>,
    main_tx: async_channel::Sender<MainEvent>,
}

/// `spawn = false` handles calls in arrival order, which player commands
/// need. The handlers only queue work, so none of them blocks the next.
#[zbus::interface(name = "net.dunevent.Phodder.Service1", spawn = false)]
impl ServiceInterface {
    async fn command(&self, command: String) {
        match serde_json::from_str::<ServiceCommand>(&command) {
            Ok(command) => {
                let _ = self.commands.send(Incoming { command, reply: None });
            }
            Err(e) => tracing::warn!("ignoring a command the service can't read: {e}"),
        }
    }

    /// Sent by a window as it closes.
    async fn goodbye(&self, #[zbus(header)] header: zbus::message::Header<'_>) {
        let Some(sender) = header.sender().map(|s| s.to_string()) else { return };
        let (hub, main_tx) = (self.hub.clone(), self.main_tx.clone());
        tokio::spawn(async move {
            if hub.forget(&sender).await {
                last_window_closed(&main_tx).await;
            }
        });
    }
}

/// Calls that wait, each handled in its own task. Results come back as
/// method replies because Flatpak's D-Bus proxy can drop signals sent from
/// one instance of an app to another, while it always passes replies.
struct CallsInterface {
    commands: tokio::sync::mpsc::UnboundedSender<Incoming>,
    hub: Arc<Hub>,
}

#[zbus::interface(name = "net.dunevent.Phodder.ServiceCalls1")]
impl CallsInterface {
    /// `UiUpdate`s as JSON, returned as soon as there are any or empty after
    /// `POLL_WAIT`. The first call registers the window.
    async fn next_updates(&self, #[zbus(header)] header: zbus::message::Header<'_>) -> Vec<String> {
        let Some(sender) = header.sender() else { return Vec::new() };
        self.hub.next_updates(&sender.to_owned().into()).await
    }

    /// Runs a command and returns its `Result<(), String>` as JSON.
    async fn request(&self, command: String) -> String {
        let result = match serde_json::from_str::<ServiceCommand>(&command) {
            Ok(command) => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let _ = self.commands.send(Incoming { command, reply: Some(reply_tx) });
                reply_rx.await.unwrap_or_else(|_| Err("the service dropped the request".into()))
            }
            Err(e) => Err(format!("the service couldn't read the request: {e}")),
        };
        serde_json::to_string(&result).unwrap_or_default()
    }
}
