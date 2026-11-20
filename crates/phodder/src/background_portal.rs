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
