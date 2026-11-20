//! Pauses playback for phone calls via ModemManager on the system bus.
//!
//! Pauses when a call rings and stays paused through the whole call (ducking
//! is unintelligible at high playback speeds). Resumes only when the call
//! terminates, and only if this watcher paused it.
//!
//! Watches the first voice-capable modem only; dual-SIM setups exposing two
//! modems, and modems added after startup, aren't covered.

use crate::player::PlayerCore;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

async fn watch_calls(core: Arc<PlayerCore>) -> anyhow::Result<()> {
    let connection = zbus::Connection::system().await?;

    let modem_path = find_first_voice_capable_modem(&connection).await?;
    let Some(modem_path) = modem_path else {
        anyhow::bail!("no modem with voice capability found");
    };
    tracing::info!("watching calls on modem {modem_path}");

    // Set only when this watcher paused, so a call ending doesn't resume
    // playback the user paused.
    let paused_by_call = Arc::new(AtomicBool::new(false));

    // ModemManager may create any number of calls; each gets its own watcher.
    let voice_proxy = zbus::Proxy::new(&connection, MODEM_MANAGER_SERVICE, modem_path.clone(), VOICE_INTERFACE).await?;
    let mut call_added_stream = voice_proxy.receive_signal("CallAdded").await?;

    // Calls already in progress at startup appear only in the `Calls`
    // property.
    if let Ok(existing_calls) = voice_proxy.get_property::<Vec<zbus::zvariant::OwnedObjectPath>>("Calls").await {
        for call_path in existing_calls {
            spawn_call_state_watcher(connection.clone(), call_path, core.clone(), paused_by_call.clone());
        }
    }

    while let Some(signal) = call_added_stream.next().await {
        let body = signal.body();
        // CallAdded(o call_path, a{sv} properties); only the path is used.
        if let Ok((call_path,)) = body.deserialize::<(zbus::zvariant::ObjectPath,)>() {
            spawn_call_state_watcher(connection.clone(), call_path.into(), core.clone(), paused_by_call.clone());
        }
    }

    Ok(())
}

async fn find_first_voice_capable_modem(
    connection: &zbus::Connection,
) -> anyhow::Result<Option<zbus::zvariant::OwnedObjectPath>> {
    let object_manager = zbus::fdo::ObjectManagerProxy::new(connection, MODEM_MANAGER_SERVICE, MODEM_MANAGER_PATH).await?;
    let managed_objects = object_manager.get_managed_objects().await?;

    for (path, interfaces) in managed_objects {
        if interfaces.contains_key(VOICE_INTERFACE) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Watches one call's `State` property.
fn spawn_call_state_watcher(
    connection: zbus::Connection,
    call_path: zbus::zvariant::OwnedObjectPath,
    core: Arc<PlayerCore>,
    paused_by_call: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let Ok(properties_proxy) = zbus::fdo::PropertiesProxy::builder(&connection)
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
                if handle_call_state(state, &core, &paused_by_call).await {
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
            if handle_call_state(state, &core, &paused_by_call).await {
                return;
            }
        }
    });
}

/// Pauses on ringing and resumes on termination if this watcher paused.
/// Every other state, including answered, leaves playback paused for the
/// whole call. Returns true once the call has terminated.
async fn handle_call_state(state: i32, core: &PlayerCore, paused_by_call: &AtomicBool) -> bool {
    match state {
        CALL_STATE_RINGING_IN => {
            tracing::info!("call ringing, pausing playback");
            match core.player.pause_if_playing().await {
                Ok(true) => paused_by_call.store(true, Ordering::SeqCst),
                Ok(false) => {}
                Err(e) => tracing::error!("failed to pause for incoming call: {e}"),
            }
            false
        }
        CALL_STATE_TERMINATED => {
            if paused_by_call.swap(false, Ordering::SeqCst) {
                tracing::info!("call ended, resuming playback");
                if let Err(e) = core.player.play().await {
                    tracing::error!("failed to resume after call ended: {e}");
                }
            }
            true
        }
        _ => false,
    }
}
