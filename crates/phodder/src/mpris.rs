//! MPRIS (`org.mpris.MediaPlayer2` and `.Player`) at the spec's object path
//! `/org/mpris/MediaPlayer2`.

use crate::player::PlayerCore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zbus::zvariant::Value;

/// Two presses of a headset's previous or next button closer than this
/// count as a double press.
const DOUBLE_PRESS: Duration = Duration::from_millis(700);

pub const MPRIS_BUS_NAME: &str = "org.mpris.MediaPlayer2.net.dunevent.Phodder";
pub const MPRIS_OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";

/// The `org.mpris.MediaPlayer2` root interface.
pub struct MprisRoot;

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl MprisRoot {
    #[zbus(property)]
    fn identity(&self) -> &str {
        "Phodder"
    }

    #[zbus(property)]
    fn desktop_entry(&self) -> &str {
        "net.dunevent.Phodder"
    }

    // True so clicking the media notification raises the window.
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }

    /// The window is a separate process, started if it isn't running.
    async fn raise(&self) -> zbus::fdo::Result<()> {
        tokio::spawn(crate::service::open_ui(None));
        Ok(())
    }

    // Quitting is the UI's Quit action; media keys and lock screens shouldn't
    // be able to trigger it.
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        vec!["file".to_string()]
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        // Empty is valid ("unspecified").
        vec![]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Button {
    Previous,
    Next,
}

/// The `org.mpris.MediaPlayer2.Player` interface, delegating to the playback
/// engine.
pub struct MprisPlayer {
    pub core: Arc<PlayerCore>,
    /// The last previous/next press, with the episode playing then.
    last_press: Mutex<Option<(Button, Instant, Option<i64>)>>,
}

impl MprisPlayer {
    pub fn new(core: Arc<PlayerCore>) -> Self {
        Self { core, last_press: Mutex::new(None) }
    }

    fn current_episode(&self) -> Option<i64> {
        self.core.player.snapshot().episode.map(|e| e.id)
    }

    /// For a press completing a double press, the episode that was playing
    /// at the first press; None for a single press. A completed double press
    /// is forgotten, so a third press starts over.
    fn double_press(&self, button: Button) -> Option<Option<i64>> {
        let mut last = self.last_press.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let first = match *last {
            Some((b, at, episode)) if b == button && now.duration_since(at) <= DOUBLE_PRESS => Some(episode),
            _ => None,
        };
        *last = if first.is_some() { None } else { Some((button, now, self.current_episode())) };
        first
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    async fn play(&self) -> zbus::fdo::Result<()> {
        tracing::info!("MPRIS Play");
        self.core.player.play().await.map_err(to_fdo_err)
    }

    async fn pause(&self) -> zbus::fdo::Result<()> {
        tracing::info!("MPRIS Pause");
        self.core.player.pause().await.map_err(to_fdo_err)
    }

    #[zbus(name = "PlayPause")]
    async fn play_pause(&self) -> zbus::fdo::Result<()> {
        tracing::info!("MPRIS PlayPause");
        self.core.player.toggle_play().await.map_err(to_fdo_err)
    }

    /// Pauses. Car stereos send Stop when switched off, and unloading the
    /// episode then lost it from the player.
    async fn stop(&self) -> zbus::fdo::Result<()> {
        tracing::info!("MPRIS Stop: pausing");
        self.core.player.pause().await.map_err(to_fdo_err)
    }

    /// Skips forward; a double press moves to the next episode instead.
    /// Headsets have only these buttons, and a stray press from one
    /// shouldn't lose the listener's place.
    async fn next(&self) -> zbus::fdo::Result<()> {
        if let Some(first_episode) = self.double_press(Button::Next) {
            if first_episode != self.current_episode() {
                // The first press's skip ran past the end and moved on already.
                tracing::info!("MPRIS Next pressed twice: already on the next episode");
                return Ok(());
            }
            tracing::info!("MPRIS Next pressed twice: next episode");
            // Undoes the first press's skip, so the episode being left keeps
            // its place.
            let skip_ms = {
                let db = self.core.db.lock().await;
                phodder_core::settings::get_setting_i64(&db, "skip_forward_ms").unwrap_or(30_000)
            };
            let _ = self.core.player.seek_by(-skip_ms).await;
            self.core.player.next().await.map(drop).map_err(to_fdo_err)
        } else {
            tracing::info!("MPRIS Next: skip forward");
            self.core.player.skip_forward().await.map_err(to_fdo_err)
        }
    }

    /// Skips back; a double press restarts the episode instead.
    async fn previous(&self) -> zbus::fdo::Result<()> {
        if self.double_press(Button::Previous).is_some() {
            tracing::info!("MPRIS Previous pressed twice: restart episode");
            self.core.player.restart().await.map_err(to_fdo_err)
        } else {
            tracing::info!("MPRIS Previous: skip back");
            self.core.player.skip_backward().await.map_err(to_fdo_err)
        }
    }

    /// Relative, in microseconds.
    async fn seek(&self, offset: i64) -> zbus::fdo::Result<()> {
        self.core.player.seek_by(offset / 1000).await.map_err(to_fdo_err)
    }

    /// Absolute, in microseconds. `track_id` isn't checked: there is one
    /// current track.
    async fn set_position(&self, _track_id: zbus::zvariant::ObjectPath<'_>, position: i64) -> zbus::fdo::Result<()> {
        self.core.player.seek_to(position.max(0) as u64 / 1000).await.map_err(to_fdo_err)
    }

    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.core.status().as_mpris_str().to_string()
    }

    /// Excluded from PropertiesChanged per the MPRIS spec; clients poll it.
    #[zbus(property)]
    fn position(&self) -> i64 {
        self.core.position_us()
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, Value<'static>> {
        self.core.mpris_metadata_dict()
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        self.core.player.snapshot().rate
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        *phodder_playback::RATE_RANGE.start()
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        *phodder_playback::RATE_RANGE.end()
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

fn to_fdo_err(e: anyhow::Error) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}
