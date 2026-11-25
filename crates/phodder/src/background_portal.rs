//! Run in Background and Launch on Startup inside Flatpak, through the
//! Background portal. The sandbox can't write the host's
//! ~/.config/autostart, so the portal does.

use futures_util::StreamExt;
use std::collections::HashMap;
use zbus::zvariant::{OwnedValue, Value};

pub(crate) fn is_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// Asks to keep running without a window, with autostart on or off. Ok(false)
/// when the user or the system refused.
pub(crate) async fn request_background(autostart: bool) -> anyhow::Result<bool> {
    let connection = zbus::Connection::session().await?;
    let token = format!("phodder_{}", std::process::id());
    let sender = connection
        .unique_name()
        .ok_or_else(|| anyhow::anyhow!("no unique bus name"))?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    let request_path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");

    // Subscribed before the call, so a fast Response can't be missed.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")?
        .member("Response")?
        .path(request_path.as_str())?
        .build();
    let mut responses = zbus::MessageStream::for_match_rule(rule, &connection, None).await?;

    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    let reason = crate::i18n::gettext("Refresh podcasts and download episodes after login");
    options.insert("reason", Value::from(reason.as_str()));
    options.insert("autostart", Value::from(autostart));
    options.insert("commandline", Value::from(vec!["phodder", "--hidden"]));
    connection
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Background"),
            "RequestBackground",
            &("", options),
        )
        .await?;

    let message = responses.next().await.ok_or_else(|| anyhow::anyhow!("portal closed without responding"))??;
    let (code, results): (u32, HashMap<String, OwnedValue>) = message.body().deserialize()?;
    if code != 0 {
        return Ok(false);
    }
    let granted = |key: &str| results.get(key).and_then(|value| bool::try_from(value).ok()).unwrap_or(false);
    Ok(granted("background") && (!autostart || granted("autostart")))
}

/// The line under Phodder in the desktop's list of background apps: what's
/// playing and what the service is doing. Fed from the service's updates and
/// sent only when it changes.
#[derive(Default)]
pub(crate) struct Status {
    state: std::sync::Mutex<StatusState>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct StatusState {
    /// (MPRIS status, title) of what's loaded.
    playback: Option<(String, String)>,
    refreshing: bool,
}

/// The portal refuses longer messages.
const MAX_STATUS_CHARS: usize = 96;

impl Status {
    pub(crate) fn observe(&self, update: &crate::ui::UiUpdate) {
        use crate::ui::UiUpdate;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match update {
            UiUpdate::PlaybackState { status, title, .. } => {
                state.playback = (status != "Stopped").then(|| (status.clone(), title.clone()));
            }
            UiUpdate::Refreshing(refreshing) => state.refreshing = *refreshing,
            UiUpdate::DownloadProgress { .. } | UiUpdate::DataChanged => {}
            _ => return,
        }
        drop(state);
        self.changed.notify_one();
    }

    fn message(&self) -> String {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let downloads = crate::downloader::active_download_count();
        let activity = if state.refreshing {
            Some(crate::i18n::gettext("Checking for new episodes"))
        } else if downloads > 0 {
            Some(crate::i18n::ngettext_f("Downloading {n} episode", "Downloading {n} episodes", downloads as u32, &[("n", &downloads.to_string())]))
        } else {
            None
        };
        let activity_chars = activity.as_ref().map_or(0, |a| a.chars().count() + 3);
        let playback = state.playback.as_ref().map(|(status, title)| {
            let title = shorten(title, MAX_STATUS_CHARS.saturating_sub(activity_chars + 10));
            if status == "Playing" {
                crate::i18n::gettext_f("Playing: {title}", &[("title", &title)])
            } else {
                crate::i18n::gettext_f("Paused: {title}", &[("title", &title)])
            }
        });
        let message = match (playback, activity) {
            (Some(playback), Some(activity)) => format!("{playback} · {activity}"),
            (Some(only), None) | (None, Some(only)) => only,
            (None, None) => crate::i18n::gettext("Idle"),
        };
        shorten(&message, MAX_STATUS_CHARS)
    }

    /// Keeps the portal's status current until the portal turns out not to
    /// support it. Downloads have no update of their own when they finish,
    /// so the count is also checked every few seconds.
    pub(crate) async fn run(self: std::sync::Arc<Self>) {
        if !is_flatpak() {
            return;
        }
        let connection = match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(e) => {
                tracing::warn!("no session bus for the background status: {e}");
                return;
            }
        };
        let mut sent = String::new();
        loop {
            let message = self.message();
            if message != sent {
                let mut options: HashMap<&str, Value> = HashMap::new();
                options.insert("message", Value::from(message.as_str()));
                let result = connection
                    .call_method(
                        Some("org.freedesktop.portal.Desktop"),
                        "/org/freedesktop/portal/desktop",
                        Some("org.freedesktop.portal.Background"),
                        "SetStatus",
                        &(options,),
                    )
                    .await;
                match result {
                    Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" => {
                        tracing::info!("this background portal has no status line");
                        return;
                    }
                    // Not retried until the message changes, so a failing
                    // portal isn't asked every few seconds.
                    Err(e) => tracing::warn!("couldn't set the background status: {e}"),
                    Ok(_) => {}
                }
                sent = message;
            }
            tokio::select! {
                _ = self.changed.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
    }
}

fn shorten(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut short: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::UiUpdate;

    fn playback(status: &str, title: &str) -> UiUpdate {
        UiUpdate::PlaybackState {
            status: status.into(),
            title: title.into(),
            podcast_title: String::new(),
            artwork_local_path: None,
            position_us: 0,
            duration_us: None,
            playback_rate: 1.0,
            sleep_timer_remaining_secs: None,
            sleep_at_episode_end: false,
            episode_id: Some(1),
            preview: false,
            has_chapters: false,
            chapter_starts_ms: Default::default(),
            chapter_title: None,
        }
    }

    #[test]
    fn the_status_names_what_plays_and_what_runs() {
        let status = Status::default();
        assert_eq!(status.message(), "Idle");
        status.observe(&playback("Playing", "An Episode"));
        assert_eq!(status.message(), "Playing: An Episode");
        status.observe(&UiUpdate::Refreshing(true));
        assert_eq!(status.message(), "Playing: An Episode · Checking for new episodes");
        status.observe(&playback("Paused", "An Episode"));
        status.observe(&UiUpdate::Refreshing(false));
        assert_eq!(status.message(), "Paused: An Episode");
        status.observe(&playback("Stopped", ""));
        assert_eq!(status.message(), "Idle");
    }

    #[test]
    fn a_long_title_is_shortened_to_fit() {
        let status = Status::default();
        status.observe(&playback("Playing", &"a".repeat(200)));
        status.observe(&UiUpdate::Refreshing(true));
        let message = status.message();
        assert!(message.chars().count() <= MAX_STATUS_CHARS, "{} chars", message.chars().count());
        assert!(message.ends_with(" · Checking for new episodes"), "{message}");
        assert!(message.contains('…'));
    }
}
