//! Pauses playback for phone calls via ModemManager on the system bus.
//!
//! Pauses when a call rings and stays paused through the whole call (ducking
//! is unintelligible at high playback speeds). Resumes when the call ends,
//! answered or not, and only if this watcher paused it. A call that ends
//! can be deleted (by the phone app) without its Terminated state arriving
//! first, so a deleted call counts as ended too.
//!
//! Watches every voice-capable modem, including ones that appear later
//! (a second SIM, a modem that powers up late, a ModemManager restart).

use crate::player::PlayerCore;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

const MODEM_MANAGER_SERVICE: &str = "org.freedesktop.ModemManager1";
const MODEM_MANAGER_PATH: &str = "/org/freedesktop/ModemManager1";
const VOICE_INTERFACE: &str = "org.freedesktop.ModemManager1.Modem.Voice";
const CALL_INTERFACE: &str = "org.freedesktop.ModemManager1.Call";

/// MMCallState values (ModemManager-enums.h).
const CALL_STATE_RINGING_IN: i32 = 3;
const CALL_STATE_TERMINATED: i32 = 7;

/// Returns at once when ModemManager or a modem isn't available, the normal
/// case on non-phone hardware.
pub fn spawn_call_watcher(core: Arc<PlayerCore>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = watch_calls(core).await {
            tracing::info!(
                "call-state watching not available (no modem/ModemManager, or a connection error): {e}"
            );
        }
    })
}

/// Shared by every modem's watcher, so a call ending on one modem doesn't
/// resume over a call on another.
#[derive(Clone)]
struct Calls {
    connection: zbus::Connection,
    core: Arc<PlayerCore>,
    /// Set only when a call paused, so a call ending doesn't resume playback
    /// the user paused.
    paused_by_call: Arc<AtomicBool>,
    modems: Arc<Mutex<HashSet<zbus::zvariant::OwnedObjectPath>>>,
}

async fn watch_calls(core: Arc<PlayerCore>) -> anyhow::Result<()> {
    let connection = zbus::Connection::system().await?;
    let calls = Calls {
        connection: connection.clone(),
        core,
        paused_by_call: Arc::new(AtomicBool::new(false)),
        modems: Arc::new(Mutex::new(HashSet::new())),
    };
    let object_manager = zbus::fdo::ObjectManagerProxy::new(&connection, MODEM_MANAGER_SERVICE, MODEM_MANAGER_PATH).await?;
    // Subscribed before listing, so a modem added in between isn't missed.
    let mut added = object_manager.receive_interfaces_added().await?;
    let mut removed = object_manager.receive_interfaces_removed().await?;
    match object_manager.get_managed_objects().await {
        Ok(objects) => {
            for (path, interfaces) in objects {
                if interfaces.contains_key(VOICE_INTERFACE) {
                    calls.watch_modem(path);
                }
            }
        }
        Err(e) => tracing::info!("ModemManager not answering yet; waiting for modems: {e}"),
    }
    loop {
        tokio::select! {
            Some(signal) = added.next() => {
                let Ok(args) = signal.args() else { continue };
                if args.interfaces_and_properties.keys().any(|name| name.to_string() == VOICE_INTERFACE) {
                    calls.watch_modem(args.object_path.to_owned().into());
                }
            }
            Some(signal) = removed.next() => {
                let Ok(args) = signal.args() else { continue };
                if args.interfaces.iter().any(|name| name.to_string() == VOICE_INTERFACE) {
                    let path: zbus::zvariant::OwnedObjectPath = args.object_path.to_owned().into();
                    calls.modems.lock().unwrap_or_else(|e| e.into_inner()).remove(&path);
                    tracing::info!("modem {path} went away");
                    calls.resume_unless_call_in_progress().await;
                }
            }
            else => break,
        }
    }
    Ok(())
}

impl Calls {
    fn watch_modem(&self, modem_path: zbus::zvariant::OwnedObjectPath) {
        if !self.modems.lock().unwrap_or_else(|e| e.into_inner()).insert(modem_path.clone()) {
            return;
        }
        tracing::info!("watching calls on modem {modem_path}");
        let calls = self.clone();
        tokio::spawn(async move {
            if let Err(e) = calls.watch_modem_calls(&modem_path).await {
                tracing::warn!("stopped watching calls on modem {modem_path}: {e}");
            }
            calls.modems.lock().unwrap_or_else(|e| e.into_inner()).remove(&modem_path);
        });
    }

    async fn watch_modem_calls(&self, modem_path: &zbus::zvariant::OwnedObjectPath) -> anyhow::Result<()> {
        // ModemManager may create any number of calls; each gets its own watcher.
        let voice_proxy = zbus::Proxy::new(&self.connection, MODEM_MANAGER_SERVICE, modem_path.clone(), VOICE_INTERFACE).await?;
        let mut call_added_stream = voice_proxy.receive_signal("CallAdded").await?;
        let mut call_deleted_stream = voice_proxy.receive_signal("CallDeleted").await?;

        // Calls already in progress appear only in the `Calls` property.
        if let Ok(existing_calls) = voice_proxy.get_property::<Vec<zbus::zvariant::OwnedObjectPath>>("Calls").await {
            for call_path in existing_calls {
                spawn_call_state_watcher(self.clone(), call_path);
            }
        }

        loop {
            if !self.modems.lock().unwrap_or_else(|e| e.into_inner()).contains(modem_path) {
                return Ok(());
            }
            tokio::select! {
                Some(signal) = call_added_stream.next() => {
                    let body = signal.body();
                    // CallAdded(o call_path, a{sv} properties); only the path is used.
                    if let Ok((call_path,)) = body.deserialize::<(zbus::zvariant::ObjectPath,)>() {
                        spawn_call_state_watcher(self.clone(), call_path.into());
                    }
                }
                Some(_) = call_deleted_stream.next() => {
                    self.resume_unless_call_in_progress().await;
                }
                else => return Ok(()),
            }
        }
    }

    async fn resume_unless_call_in_progress(&self) {
        let modems: Vec<_> = self.modems.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
        for modem_path in modems {
            if any_call_in_progress(&self.connection, modem_path).await {
                return;
            }
        }
        resume_if_paused_by_call(&self.core, &self.paused_by_call).await;
    }
}

/// Whether any remaining call hasn't terminated. Unknown states count as in
/// progress, so a call that can't be read doesn't resume over itself.
async fn any_call_in_progress(connection: &zbus::Connection, modem_path: zbus::zvariant::OwnedObjectPath) -> bool {
    let calls = async {
        let voice_proxy = zbus::Proxy::new(connection, MODEM_MANAGER_SERVICE, modem_path, VOICE_INTERFACE).await?;
        voice_proxy.get_property::<Vec<zbus::zvariant::OwnedObjectPath>>("Calls").await
    };
    let Ok(calls) = calls.await else {
        return true;
    };
    for call_path in calls {
        let state = async {
            let proxy = zbus::Proxy::new(connection, MODEM_MANAGER_SERVICE, call_path, CALL_INTERFACE).await?;
            proxy.get_property::<i32>("State").await
        };
        match state.await {
            Ok(CALL_STATE_TERMINATED) => {}
            _ => return true,
        }
    }
    false
}

async fn resume_if_paused_by_call(core: &PlayerCore, paused_by_call: &AtomicBool) {
    if paused_by_call.swap(false, Ordering::SeqCst) {
        tracing::info!("call ended, resuming playback");
        if let Err(e) = core.player.play().await {
            tracing::error!("failed to resume after call ended: {e}");
        }
    }
}

/// Watches one call's `State` property.
fn spawn_call_state_watcher(calls: Calls, call_path: zbus::zvariant::OwnedObjectPath) {
    tokio::spawn(async move {
        let connection = &calls.connection;
        let Ok(properties_proxy) = zbus::fdo::PropertiesProxy::builder(connection)
            .destination(MODEM_MANAGER_SERVICE)
            .unwrap_or_else(|_| unreachable!("static destination string is always valid"))
            .path(call_path.clone())
            .unwrap_or_else(|_| unreachable!("path from ObjectManager is always valid"))
            .build()
            .await
        else {
            tracing::warn!("failed to watch call {call_path}: couldn't build Properties proxy");
            return;
        };

        let Ok(mut changes) = properties_proxy.receive_properties_changed().await else {
            tracing::warn!("failed to subscribe to PropertiesChanged for call {call_path}");
            return;
        };

        // An incoming call is created already ringing, so its current
        // state is handled before waiting for changes.
        if let Ok(value) = properties_proxy.get(CALL_INTERFACE.try_into().expect("valid interface name"), "State").await {
            if let Ok(state) = i32::try_from(&value) {
                if handle_call_state(state, &calls).await {
                    return;
                }
            }
        }
        while let Some(change) = changes.next().await {
            let Ok(args) = change.args() else { continue };
            if args.interface_name.as_str() != CALL_INTERFACE {
                continue;
            }
            let Some(state_value) = args.changed_properties.get("State") else { continue };
            // MMCallState is sent as a signed int ("i"); u32::try_from
            // rejected every value.
            let Ok(state) = i32::try_from(state_value) else { continue };
            if handle_call_state(state, &calls).await {
                return;
            }
        }
    });
}

/// Pauses on ringing and resumes on termination if this watcher paused.
/// Every other state, including answered, leaves playback paused for the
/// whole call. Returns true once the call has terminated.
async fn handle_call_state(state: i32, calls: &Calls) -> bool {
    match state {
        CALL_STATE_RINGING_IN => {
            tracing::info!("call ringing, pausing playback");
            match calls.core.player.pause_if_playing().await {
                Ok(true) => calls.paused_by_call.store(true, Ordering::SeqCst),
                Ok(false) => {}
                Err(e) => tracing::error!("failed to pause for incoming call: {e}"),
            }
            false
        }
        CALL_STATE_TERMINATED => {
            calls.resume_unless_call_in_progress().await;
            true
        }
        _ => false,
    }
}
