//! Whether Phodder owns its MPRIS name. It gives the name up while another
//! player plays: BlueZ, through mpris-proxy, sends a headset's buttons to
//! the first registered player whether or not it is playing, so a paused
//! Phodder would otherwise take presses meant for the other player.

use crate::mpris::MPRIS_BUS_NAME;
use tokio::sync::Mutex;

pub struct Presence {
    connection: zbus::Connection,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    present: bool,
    /// The player Phodder stepped aside for; its leaving the bus brings
    /// Phodder back.
    displaced_by: Option<String>,
}

impl Presence {
    pub fn new(connection: zbus::Connection) -> Self {
        Self { connection, state: Mutex::new(State::default()) }
    }

    /// Gives up the name because `peer` started playing.
    pub async fn step_aside(&self, peer: &str) {
        let mut state = self.state.lock().await;
        state.displaced_by = Some(peer.to_string());
        if !state.present {
            return;
        }
        match self.connection.release_name(MPRIS_BUS_NAME).await {
            Ok(_) => {
                state.present = false;
                tracing::info!("{peer} is playing; giving up the MPRIS name");
            }
            Err(e) => tracing::warn!("couldn't give up the MPRIS name for {peer}: {e}"),
        }
    }

    /// Takes the name, because Phodder is playing or nothing displaces it.
    pub async fn come_back(&self, reason: &str) {
        let mut state = self.state.lock().await;
        state.displaced_by = None;
        if state.present {
            return;
        }
        match self.connection.request_name(MPRIS_BUS_NAME).await {
            Ok(_) => {
                state.present = true;
                tracing::info!("taking the MPRIS name: {reason}");
            }
            Err(e) => tracing::warn!("couldn't take the MPRIS name ({reason}): {e}"),
        }
    }

    /// Brings Phodder back when the player it stepped aside for quits.
    /// Another player merely pausing doesn't: the next press should resume
    /// that player, not start Phodder.
    pub async fn peer_left(&self, peer: &str) {
        let displaced_by_peer = self.state.lock().await.displaced_by.as_deref() == Some(peer);
        if displaced_by_peer {
            self.come_back(&format!("{peer} quit")).await;
        }
    }
}
