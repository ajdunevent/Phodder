//! HTTP side of gpodder-API sync for both backends, using the data shapes in
//! phodder_core::gpodder_sync. Both authenticate with HTTP Basic Auth on
//! every request: gpodder.net with username and password, Nextcloud with a
//! login name and a Login Flow v2 app password.

use phodder_core::gpodder_sync::{
    EpisodeActionRecord, EpisodeActionsResponse, EpisodeActionsUploadResponse, SubscriptionChangeRequest, SubscriptionChangeResponse,
};

/// A gpodder-API service and its credentials.
#[derive(Debug, Clone)]
pub enum GpodderAccount {
    /// `base_url` is configurable: self-hosted mygpo instances implement the
    /// same API.
    GpodderNet { base_url: String, username: String, password: String, device_id: String },
    /// No device_id: Nextcloud's GPodder Sync app has one subscription list
    /// per account, shared by every device.
    Nextcloud { server: String, login_name: String, app_password: String },
}

impl GpodderAccount {
    fn basic_auth_credentials(&self) -> (&str, &str) {
        match self {
            GpodderAccount::GpodderNet { username, password, .. } => (username, password),
            GpodderAccount::Nextcloud { login_name, app_password, .. } => (login_name, app_password),
        }
    }

    /// Subscription changes since `since` (a Unix timestamp), or the full
    /// list when `None`.
    pub async fn get_subscription_changes(&self, since: Option<i64>) -> anyhow::Result<SubscriptionChangeResponse> {
        let (username_or_login, password) = self.basic_auth_credentials();
        let url = match self {
            GpodderAccount::GpodderNet { base_url, username, device_id, .. } => {
                format!("{}/api/2/subscriptions/{username}/{device_id}.json", base_url.trim_end_matches('/'))
            }
            GpodderAccount::Nextcloud { server, .. } => {
                format!("{}/index.php/apps/gpoddersync/subscriptions", server.trim_end_matches('/'))
            }
        };
        let mut request = reqwest::Client::new().get(&url).basic_auth(username_or_login, Some(password));
        if let Some(since) = since {
            request = request.query(&[("since", since)]);
        }
        let response = request.send().await?.error_for_status()?;
        Ok(response.json::<SubscriptionChangeResponse>().await?)
    }

    /// Uploads local subscription changes. gpodder.net's response includes
    /// remote changes; Nextcloud's returns only a timestamp, so its
    /// add/remove lists are empty and callers fetch remote state with
    /// `get_subscription_changes`.
    pub async fn upload_subscription_changes(&self, add: &[String], remove: &[String]) -> anyhow::Result<SubscriptionChangeResponse> {
        let (username_or_login, password) = self.basic_auth_credentials();
        let url = match self {
            GpodderAccount::GpodderNet { base_url, username, device_id, .. } => {
                format!("{}/api/2/subscriptions/{username}/{device_id}.json", base_url.trim_end_matches('/'))
            }
            GpodderAccount::Nextcloud { server, .. } => {
                format!("{}/index.php/apps/gpoddersync/subscription_change/create", server.trim_end_matches('/'))
            }
        };
        let body = SubscriptionChangeRequest { add: add.to_vec(), remove: remove.to_vec() };
        let response =
            reqwest::Client::new().post(&url).basic_auth(username_or_login, Some(password)).json(&body).send().await?.error_for_status()?;
        Ok(response.json::<SubscriptionChangeResponse>().await?)
    }

    /// Episode actions since `since` (a Unix timestamp), or all of them when
    /// `None`.
    pub async fn get_episode_actions(&self, since: Option<i64>) -> anyhow::Result<EpisodeActionsResponse> {
        let (username_or_login, password) = self.basic_auth_credentials();
        let url = match self {
            GpodderAccount::GpodderNet { base_url, username, .. } => {
                format!("{}/api/2/episodes/{username}.json", base_url.trim_end_matches('/'))
            }
            GpodderAccount::Nextcloud { server, .. } => {
                format!("{}/index.php/apps/gpoddersync/episode_action", server.trim_end_matches('/'))
            }
        };
        let mut request = reqwest::Client::new().get(&url).basic_auth(username_or_login, Some(password));
        if let Some(since) = since {
            request = request.query(&[("since", since)]);
        }
        let response = request.send().await?.error_for_status()?;
        Ok(response.json::<EpisodeActionsResponse>().await?)
    }

    pub async fn upload_episode_actions(&self, actions: &[EpisodeActionRecord]) -> anyhow::Result<EpisodeActionsUploadResponse> {
        let (username_or_login, password) = self.basic_auth_credentials();
        let url = match self {
            GpodderAccount::GpodderNet { base_url, username, .. } => {
                format!("{}/api/2/episodes/{username}.json", base_url.trim_end_matches('/'))
            }
            GpodderAccount::Nextcloud { server, .. } => {
                format!("{}/index.php/apps/gpoddersync/episode_action/create", server.trim_end_matches('/'))
            }
        };
        // gpodder.net records which device listened, and documents
        // timestamps as UTC without an offset or fractions.
        let actions: Vec<EpisodeActionRecord> = match self {
            GpodderAccount::GpodderNet { device_id, .. } => actions
                .iter()
                .cloned()
                .map(|a| EpisodeActionRecord { device: Some(device_id.clone()), timestamp: gpodder_net_timestamp(&a.timestamp), ..a })
                .collect(),
            GpodderAccount::Nextcloud { .. } => actions.to_vec(),
        };
        let response =
            reqwest::Client::new().post(&url).basic_auth(username_or_login, Some(password)).json(&actions).send().await?.error_for_status()?;
        Ok(response.json::<EpisodeActionsUploadResponse>().await?)
    }
}

/// "2026-11-17T16:59:08.06+00:00" to "2026-11-17T16:59:08". Unparseable
/// values pass through.
fn gpodder_net_timestamp(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|t| t.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_else(|_| timestamp.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn gpodder_net_timestamps_are_utc_without_fractions() {
        assert_eq!(super::gpodder_net_timestamp("2026-11-17T10:59:08.064991814-06:00"), "2026-11-17T16:59:08");
        assert_eq!(super::gpodder_net_timestamp("2009-12-12T09:00:00"), "2009-12-12T09:00:00");
    }
}

/// Why connecting to gpodder.net failed.
#[derive(Debug)]
pub enum ConnectError {
    /// The server answered 401.
    WrongCredentials,
    Other(anyhow::Error),
}

impl From<reqwest::Error> for ConnectError {
    fn from(e: reqwest::Error) -> Self {
        ConnectError::Other(e.into())
    }
}

#[derive(serde::Deserialize)]
struct Device {
    id: String,
}

/// Checks the username and password, registers this device, and joins it
/// to the account's other devices so their subscriptions are shared:
/// gpodder.net keeps a subscription list per device and only merges
/// devices in a sync group.
pub async fn connect_gpodder_net(base_url: &str, username: &str, password: &str, device_id: &str) -> Result<(), ConnectError> {
    let base = base_url.trim_end_matches('/');
    let client = reqwest::Client::new();

    let login = client.post(format!("{base}/api/2/auth/{username}/login.json")).basic_auth(username, Some(password)).send().await?;
    if login.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ConnectError::WrongCredentials);
    }
    login.error_for_status()?;

    let devices: Vec<Device> =
        client.get(format!("{base}/api/2/devices/{username}.json")).basic_auth(username, Some(password)).send().await?.error_for_status()?.json().await?;

    client
        .post(format!("{base}/api/2/devices/{username}/{device_id}.json"))
        .basic_auth(username, Some(password))
        .json(&serde_json::json!({ "caption": "Phodder", "type": "mobile" }))
        .send()
        .await?
        .error_for_status()?;

    let others: Vec<String> = devices.into_iter().map(|d| d.id).filter(|id| id != device_id).collect();
    if !others.is_empty() {
        let mut group = vec![device_id.to_string()];
        group.extend(others);
        client
            .post(format!("{base}/api/2/sync-devices/{username}.json"))
            .basic_auth(username, Some(password))
            .json(&serde_json::json!({ "synchronize": [group], "stop-synchronize": [] }))
            .send()
            .await?
            .error_for_status()?;
    }
    Ok(())
}
