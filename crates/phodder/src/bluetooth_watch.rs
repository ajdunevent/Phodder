//! Pauses when Bluetooth audio goes away and resumes when the same device
//! is back.
//!
//! The main signal is where the stream is playing: the sound server names a
//! Bluetooth output after the device (`bluez_output.AA_BB_CC_DD_EE_FF.1`,
//! `bluez_sink.AA_BB_CC_DD_EE_FF.a2dp_sink` on PulseAudio) and moves the
//! stream to another output the moment the device's audio goes. BlueZ can
//! take up to ~20 s to report an out-of-range device as disconnected, and
//! "connected" comes before the audio link is up, so BlueZ alone either
//! paused late (the episode carried on through the speaker) or resumed too
//! early. BlueZ is still watched: it decides when the output can't be read
//! (pipewiresink), and a reconnection starts the wait for the output.
//!
//! Only playback this watcher paused is resumed, and only by the device it
//! was paused for.

use crate::player::PlayerCore;
use futures_util::StreamExt;
use phodder_playback::Status;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BLUEZ_SERVICE: &str = "org.bluez";
const DEVICE_INTERFACE: &str = "org.bluez.Device1";

/// How long a reconnected device's audio output may take to appear before
/// playback resumes anyway.
const RECONNECT_WAIT: Duration = Duration::from_secs(8);

/// Audio service UUIDs: A2DP sink, Headset, Hands-Free.
const AUDIO_UUIDS: [&str; 3] = ["0000110b-0000-1000-8000-00805f9b34fb", "00001108-0000-1000-8000-00805f9b34fb", "0000111e-0000-1000-8000-00805f9b34fb"];

#[derive(Default)]
struct State {
    /// The device (address in `AA_BB_CC_DD_EE_FF` form) whose loss paused
    /// playback.
    paused_for: Option<String>,
}

pub fn spawn_bluetooth_watcher(core: Arc<PlayerCore>) -> tokio::task::JoinHandle<()> {
    let state = Arc::new(Mutex::new(State::default()));
    tokio::spawn(watch_output(core.clone(), state.clone()));
    tokio::spawn(async move {
        if let Err(e) = watch_bluez(core, state).await {
            // Normal on hardware without Bluetooth.
            tracing::info!("Bluetooth watching not available (no BlueZ, or a connection error): {e}");
        }
    })
}

/// The device address in a Bluetooth output's name.
fn bluetooth_address(output: &str) -> Option<String> {
    let rest = output.strip_prefix("bluez_output.").or_else(|| output.strip_prefix("bluez_sink."))?;
    let address = rest.split('.').next()?;
    (address.len() == 17).then(|| address.to_uppercase())
}

/// `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF` → `AA_BB_CC_DD_EE_FF`.
fn device_address(path: &str) -> Option<String> {
    let (_, dev) = path.rsplit_once("/dev_")?;
    (dev.len() == 17 && !dev.contains('/')).then(|| dev.to_uppercase())
}

/// Follows the stream's output: playing audio that leaves a Bluetooth
/// output pauses, and a paused stream back on that device resumes.
async fn watch_output(core: Arc<PlayerCore>, state: Arc<Mutex<State>>) {
    let mut last_output: Option<String> = None;
    loop {
        // Moves while playing are signalled; a paused stream's aren't, so
        // it's read every second as well.
        tokio::select! {
            _ = core.player.output_changed() => {}
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        let output = core.player.output_device();
        let status = core.player.status();

        let waiting_for = state.lock().unwrap().paused_for.clone();
        if let Some(address) = waiting_for {
            if status == Status::Playing {
                // The user carried on elsewhere; nothing to resume.
                state.lock().unwrap().paused_for = None;
            } else if output.as_deref().and_then(bluetooth_address).as_deref() == Some(address.as_str()) {
                tracing::info!("audio is back on Bluetooth device {address}, resuming");
                resume(&core, &state, &address).await;
            }
        }

        if output != last_output {
            tracing::debug!("audio output: {:?} -> {:?}", last_output, output);
            let left = last_output.as_deref().and_then(bluetooth_address);
            // None means the stream ended (episode change or stop), not that
            // the device went.
            if let (Some(address), Some(now)) = (left, output.as_deref()) {
                if bluetooth_address(now).as_deref() != Some(address.as_str()) {
                    match core.player.pause_if_playing().await {
                        Ok(true) => {
                            tracing::info!("audio left Bluetooth device {address} for {now}, paused");
                            state.lock().unwrap().paused_for = Some(address);
                        }
                        Ok(false) => {}
                        Err(e) => tracing::error!("failed to pause when audio left Bluetooth: {e}"),
                    }
                }
            }
            last_output = output;
        }
    }
}

/// Resumes once for `address`; a resume already done by the other path is
/// a no-op. Three attempts a second apart, for a link still settling.
async fn resume(core: &Arc<PlayerCore>, state: &Arc<Mutex<State>>, address: &str) {
    {
        let mut state = state.lock().unwrap();
        if state.paused_for.as_deref() != Some(address) {
            return;
        }
        state.paused_for = None;
    }
    for attempt in 1..=3 {
        match core.player.play().await {
            Ok(()) => return,
            Err(e) if attempt < 3 => {
                tracing::warn!("resume attempt {attempt}/3 for Bluetooth device {address} failed, retrying in 1s: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => tracing::error!("failed to resume for Bluetooth device {address} after 3 attempts: {e}"),
        }
    }
}

async fn watch_bluez(core: Arc<PlayerCore>, state: Arc<Mutex<State>>) -> anyhow::Result<()> {
    let connection = zbus::Connection::system().await?;
    // Every device's changes, including ones paired later and ones whose
    // details arrive after pairing; whether it's an audio device is checked
    // when it matters.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(BLUEZ_SERVICE)?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path_namespace("/org/bluez")?
        .build();
    let mut changes = zbus::MessageStream::for_match_rule(rule, &connection, None).await?;
    tracing::info!("watching Bluetooth devices");

    while let Some(message) = changes.next().await {
        let Ok(message) = message else { continue };
        let Some(path) = message.header().path().map(|p| p.to_string()) else { continue };
        let Ok((interface, changed, _)) = message.body().deserialize::<(String, HashMap<String, zbus::zvariant::OwnedValue>, Vec<String>)>() else {
            continue;
        };
        if interface != DEVICE_INTERFACE {
            continue;
        }
        let Some(address) = device_address(&path) else { continue };
        let connected = changed.get("Connected").and_then(|v| bool::try_from(&**v).ok());
        let services_resolved = changed.get("ServicesResolved").and_then(|v| bool::try_from(&**v).ok());

        if connected == Some(false) {
            on_disconnected(&core, &state, &connection, &path, &address).await;
        } else if connected == Some(true) || services_resolved == Some(true) {
            let waiting = state.lock().unwrap().paused_for.as_deref() == Some(address.as_str());
            if waiting {
                tokio::spawn(resume_after_reconnect(core.clone(), state.clone(), address));
            }
        }
    }
    Ok(())
}

async fn on_disconnected(core: &Arc<PlayerCore>, state: &Arc<Mutex<State>>, connection: &zbus::Connection, path: &str, address: &str) {
    if core.player.status() != Status::Playing {
        return;
    }
    let output = core.player.output_device();
    let applies = match output.as_deref() {
        // The output says where audio is going; a device that isn't it
        // doesn't matter (output_watch handles one that was).
        Some(output) => bluetooth_address(output).as_deref() == Some(address),
        // Can't tell, so any audio device counts.
        None => is_audio_device(connection, path).await,
    };
    if !applies {
        return;
    }
    match core.player.pause_if_playing().await {
        Ok(true) => {
            tracing::info!("Bluetooth audio device {address} disconnected, paused");
            state.lock().unwrap().paused_for = Some(address.to_string());
        }
        Ok(false) => {}
        Err(e) => tracing::error!("failed to pause after Bluetooth disconnect: {e}"),
    }
}

/// Waits for the device's output to carry the paused stream, then resumes;
/// after RECONNECT_WAIT (or at once when the output can't be read) it
/// resumes anyway. "Connected" arrives before the audio link is up, and
/// resuming then played through the speaker or failed.
async fn resume_after_reconnect(core: Arc<PlayerCore>, state: Arc<Mutex<State>>, address: String) {
    let readable = core.player.output_device().is_some();
    let deadline = tokio::time::Instant::now() + if readable { RECONNECT_WAIT } else { Duration::from_secs(2) };
    while tokio::time::Instant::now() < deadline {
        if state.lock().unwrap().paused_for.as_deref() != Some(address.as_str()) {
            // Resumed by output_watch, or the user took over.
            return;
        }
        if core.player.output_device().as_deref().and_then(bluetooth_address).as_deref() == Some(address.as_str()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tracing::info!("Bluetooth audio device {address} reconnected, resuming");
    resume(&core, &state, &address).await;
}

async fn is_audio_device(connection: &zbus::Connection, path: &str) -> bool {
    let Ok(proxy) = zbus::fdo::PropertiesProxy::builder(connection).destination(BLUEZ_SERVICE).and_then(|b| b.path(path.to_string())) else {
        return false;
    };
    let Ok(proxy) = proxy.build().await else { return false };
    let Ok(interface) = zbus::names::InterfaceName::try_from(DEVICE_INTERFACE) else { return false };
    let Ok(properties) = proxy.get_all(Some(interface).into()).await else { return false };
    let icon = properties.get("Icon").and_then(|v| String::try_from(&**v).ok()).unwrap_or_default();
    let uuids: Vec<String> = properties.get("UUIDs").and_then(|v| v.try_clone().ok()).and_then(|v| Vec::<String>::try_from(v).ok()).unwrap_or_default();
    // BlueZ names headsets audio-headset and headphones audio-headphones;
    // only "audio-card" used to count, so most earbuds were ignored.
    icon.starts_with("audio-") || uuids.iter().any(|u| AUDIO_UUIDS.contains(&u.to_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_and_devices_share_an_address() {
        assert_eq!(bluetooth_address("bluez_output.aa_bb_cc_dd_ee_ff.1").as_deref(), Some("AA_BB_CC_DD_EE_FF"));
        assert_eq!(bluetooth_address("bluez_sink.AA_BB_CC_DD_EE_FF.a2dp_sink").as_deref(), Some("AA_BB_CC_DD_EE_FF"));
        assert_eq!(bluetooth_address("alsa_output.platform-sound.HiFi__Speaker__sink"), None);
        assert_eq!(device_address("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").as_deref(), Some("AA_BB_CC_DD_EE_FF"));
        assert_eq!(device_address("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/sep1/fd0"), None);
        assert_eq!(device_address("/org/bluez/hci0"), None);
    }
}
