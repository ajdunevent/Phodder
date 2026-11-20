//! Secrets in the freedesktop Secret Service, via oo7: sync account
//! passwords and the Podcast Index API secret. The database stores only the
//! `credential_ref` lookup key, never the secret.

use zeroize::Zeroizing;

const KEYRING_SERVICE: &str = "net.dunevent.Phodder";

/// oo7's AsAttributes requires Self: Sized, so call sites pass `&attrs`
/// (this array), not `&attrs[..]`.
fn keyring_attributes(credential_ref: &str) -> [(&'static str, &str); 2] {
    [("service", KEYRING_SERVICE), ("credential_ref", credential_ref)]
}

/// Derived from (kind, server, login_name), so it stays the same when an
/// account is removed and added again. `server` is None for gpodder.net and
/// becomes an empty segment, keeping the key three-part.
pub fn credential_ref_for(kind: &str, server: Option<&str>, login_name: &str) -> String {
    let server = server.unwrap_or("").trim_end_matches('/');
    format!("{kind}:{server}:{login_name}")
}

/// Stores a sync account's secret, replacing any existing one for this
/// credential_ref.
pub async fn store_credential(credential_ref: &str, secret: &str) -> Result<(), String> {
    store_labeled_credential(credential_ref, "Phodder GPodder Sync Credential", secret).await
}

/// `label` is what keyring managers such as Seahorse show for the item.
pub async fn store_labeled_credential(credential_ref: &str, label: &str, secret: &str) -> Result<(), String> {
    let keyring = oo7::Keyring::new().await.map_err(|e| e.to_string())?;
    let attrs = keyring_attributes(credential_ref);
    keyring
        .create_item(
            label,
            &attrs,
            Zeroizing::new(secret.as_bytes().to_vec()),
            true, // Re-authentication overwrites the old item.
        )
        .await
        .map_err(|e| e.to_string())
}

/// `None` when nothing is stored for this credential_ref. Wrapped in
/// Zeroizing at once: converting oo7's secret to a String makes a copy that
/// oo7's own zeroizing doesn't cover.
pub async fn get_credential(credential_ref: &str) -> Result<Option<Zeroizing<String>>, String> {
    let keyring = oo7::Keyring::new().await.map_err(|e| e.to_string())?;
    let attrs = keyring_attributes(credential_ref);
    let items = keyring.search_items(&attrs).await.map_err(|e| e.to_string())?;
    let Some(item) = items.first() else { return Ok(None) };
    let secret = item.secret().await.map_err(|e| e.to_string())?;
    String::from_utf8(secret.as_bytes().to_vec())
        .map(|s| Some(Zeroizing::new(s)))
        .map_err(|e| format!("stored credential wasn't valid UTF-8: {e}"))
}

/// oo7 treats zero matching items as success, so this is safe to call when
/// nothing was stored.
pub async fn delete_credential(credential_ref: &str) -> Result<(), String> {
    let keyring = oo7::Keyring::new().await.map_err(|e| e.to_string())?;
    let attrs = keyring_attributes(credential_ref);
    keyring.delete(&attrs).await.map_err(|e| e.to_string())
}
