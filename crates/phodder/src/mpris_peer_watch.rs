//! Pauses this player, and gives up its MPRIS name, when another MPRIS
//! player on the session bus starts playing. Another app pausing isn't a
//! sign the user wants Phodder back; that app quitting gives the name back.

use crate::mpris::MPRIS_BUS_NAME;
use crate::player::PlayerCore;
use futures_util::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

/// Uses the session-bus connection MPRIS is registered on.
pub fn spawn_mpris_peer_watcher(core: Arc<PlayerCore>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = watch_peers(core).await {
            tracing::warn!("MPRIS peer watching stopped: {e}");
        }
    })
}

async fn watch_peers(core: Arc<PlayerCore>) -> anyhow::Result<()> {
    let connection = core.connection.clone();
    let dbus_proxy = zbus::fdo::DBusProxy::new(&connection).await?;

    // Peers already watched; the initial scan and a later NameOwnerChanged
    // can both report the same name.
    let watched: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // Peers already on the bus at startup.
    let names = dbus_proxy.list_names().await?;
    for name in names {
        maybe_watch_peer(&connection, name.as_str(), &core, &watched).await;
    }

    // Peers launched later.
    let mut owner_changes = dbus_proxy.receive_name_owner_changed().await?;
    while let Some(signal) = owner_changes.next().await {
        let Ok(args) = signal.args() else { continue };
        let name = args.name().as_str();
        if args.new_owner().is_some() {
            maybe_watch_peer(&connection, name, &core, &watched).await;
        } else if args.old_owner().is_some() && name != MPRIS_BUS_NAME && name.starts_with(MPRIS_PREFIX) {
            core.presence.peer_left(name).await;
        }
    }

    Ok(())
}

async fn maybe_watch_peer(
    connection: &zbus::Connection,
    name: &str,
    core: &Arc<PlayerCore>,
    watched: &Arc<Mutex<HashSet<String>>>,
) {
    if name == MPRIS_BUS_NAME || !name.starts_with(MPRIS_PREFIX) {
        return; // Not an MPRIS peer, or this app.
    }
    {
        let mut watched = watched.lock().await;
        if !watched.insert(name.to_string()) {
            return; // Already watched.
        }
    }

    tracing::debug!("watching MPRIS peer {name} for PlaybackStatus changes");
    spawn_single_peer_watcher(connection.clone(), name.to_string(), core.clone());
}

fn spawn_single_peer_watcher(connection: zbus::Connection, peer_name: String, core: Arc<PlayerCore>) {
    tokio::spawn(async move {
        let Ok(properties_proxy) = zbus::fdo::PropertiesProxy::builder(&connection)
            .destination(peer_name.clone())
            .unwrap_or_else(|_| unreachable!("a bus name from ListNames is always a valid destination"))
            .path("/org/mpris/MediaPlayer2")
            .unwrap_or_else(|_| unreachable!("static path is always valid"))
            .build()
            .await
        else {
            tracing::warn!("failed to watch MPRIS peer {peer_name}: couldn't build Properties proxy");
            return;
        };

        let Ok(mut changes) = properties_proxy.receive_properties_changed().await else {
            tracing::warn!("failed to subscribe to PropertiesChanged for MPRIS peer {peer_name}");
            return;
        };

        while let Some(change) = changes.next().await {
            let Ok(args) = change.args() else { continue };
            if args.interface_name.as_str() != MPRIS_PLAYER_INTERFACE {
                continue;
            }
            let Some(status_value) = args.changed_properties.get("PlaybackStatus") else { continue };
            let Ok(status) = String::try_from(status_value) else { continue };

            if status == "Playing" {
                tracing::info!("MPRIS peer {peer_name} started playing, pausing self");
                if let Err(e) = core.player.pause_if_playing().await {
                    tracing::error!("failed to pause for MPRIS peer {peer_name}: {e}");
                }
                core.presence.step_aside(&peer_name).await;
            }
        }
    });
}

/// Another MPRIS player that is playing right now, if any.
pub async fn playing_peer(connection: &zbus::Connection) -> Option<String> {
    let names = zbus::fdo::DBusProxy::new(connection).await.ok()?.list_names().await.ok()?;
    for name in names {
        let name = name.as_str();
        if name == MPRIS_BUS_NAME || !name.starts_with(MPRIS_PREFIX) {
            continue;
        }
        let status = async {
            let properties = zbus::fdo::PropertiesProxy::builder(connection).destination(name)?.path("/org/mpris/MediaPlayer2")?.build().await?;
            let value = properties.get(zbus::names::InterfaceName::from_static_str_unchecked(MPRIS_PLAYER_INTERFACE), "PlaybackStatus").await?;
            Ok::<_, zbus::Error>(String::try_from(value).unwrap_or_default())
        };
        if let Ok(Ok(status)) = tokio::time::timeout(std::time::Duration::from_millis(500), status).await {
            if status == "Playing" {
                return Some(name.to_string());
            }
        }
    }
    None
}
