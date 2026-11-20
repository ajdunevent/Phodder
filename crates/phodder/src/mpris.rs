//! MPRIS (`org.mpris.MediaPlayer2` and `.Player`) at the spec's object path
//! `/org/mpris/MediaPlayer2`.

use crate::player::PlayerCore;
use std::collections::HashMap;
use std::sync::Arc;
use zbus::zvariant::Value;

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

/// The `org.mpris.MediaPlayer2.Player` interface, delegating to the playback
/// engine.
pub struct MprisPlayer {
    pub core: Arc<PlayerCore>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    async fn play(&self) -> zbus::fdo::Result<()> {
        self.core.player.play().await.map_err(to_fdo_err)
    }

    async fn pause(&self) -> zbus::fdo::Result<()> {
        self.core.player.pause().await.map_err(to_fdo_err)
    }

    #[zbus(name = "PlayPause")]
    async fn play_pause(&self) -> zbus::fdo::Result<()> {
        self.core.player.toggle_play().await.map_err(to_fdo_err)
    }

    async fn stop(&self) -> zbus::fdo::Result<()> {
        self.core.player.stop().await.map_err(to_fdo_err)
    }

    /// Stops when nothing else is queued.
    async fn next(&self) -> zbus::fdo::Result<()> {
        self.core.player.next().await.map(drop).map_err(to_fdo_err)
    }

    /// Restarts the current episode; the queue has no history to go back to.
    async fn previous(&self) -> zbus::fdo::Result<()> {
        self.core.player.restart().await.map_err(to_fdo_err)
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
