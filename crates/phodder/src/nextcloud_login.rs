//! Nextcloud Login Flow v2
//! (docs.nextcloud.com/server/stable/developer_manual/client_apis/LoginFlow/index.html#login-flow-v2).
//! The server's login page opens in the user's own browser, so 2FA, SSO and
//! client certificates work without a webview; the app polls a separate
//! endpoint for the resulting app password.

use std::time::Duration;

/// nc_py_api uses the same 1-second step for this flow.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The server's documented token lifetime; polling past it can't succeed.
const TOKEN_LIFETIME: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone)]
pub struct LoginFlowStart {
    pub login_url: String,
    poll_token: String,
    poll_endpoint: String,
}

#[derive(Debug, Clone)]
pub struct LoginFlowResult {
    pub server: String,
    pub login_name: String,
    pub app_password: String,
}

#[derive(Debug)]
pub enum LoginFlowError {
    /// Couldn't reach the server.
    StartRequestFailed(String),
    /// Not a 200, or the body wasn't the expected JSON. Usually not a
    /// Nextcloud/ownCloud server, or one too old for Login Flow v2.
    UnexpectedStartResponse(String),
    /// No successful poll within the token lifetime.
    TimedOut,
    /// Stopped by the caller (the dialog was closed).
    Cancelled,
}

impl std::fmt::Display for LoginFlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoginFlowError::StartRequestFailed(e) => write!(f, "couldn't reach the server: {e}"),
            LoginFlowError::UnexpectedStartResponse(e) => {
                write!(f, "server didn't respond the way a Nextcloud/ownCloud server should: {e}")
            }
            LoginFlowError::TimedOut => write!(f, "login wasn't completed in time (20 minutes) -- try again"),
            LoginFlowError::Cancelled => write!(f, "login cancelled"),
        }
    }
}

/// Response to POST /index.php/login/v2.
#[derive(serde::Deserialize)]
struct StartResponse {
    poll: PollInfo,
    login: String,
}

#[derive(serde::Deserialize)]
struct PollInfo {
    token: String,
    endpoint: String,
}

/// A completed (200) poll response. Not-yet-complete polls return 404 and
/// never reach this type.
#[derive(serde::Deserialize)]
struct PollSuccessResponse {
    server: String,
    #[serde(rename = "loginName")]
    login_name: String,
    #[serde(rename = "appPassword")]
    app_password: String,
}

/// Starts a Login Flow v2 session. The caller opens the browser.
///
/// `server_url` is trimmed, a trailing slash is dropped, and a missing scheme
/// becomes https, so "example.com" works.
pub async fn start(server_url: &str) -> Result<LoginFlowStart, LoginFlowError> {
    // A bare hostname otherwise fails with a low-level URL error. Defaults to
    // https; an explicit http:// is left alone. Schemes are case-insensitive
    // (RFC 3986).
    let trimmed = server_url.trim();
    let with_scheme = if trimmed.to_ascii_lowercase().starts_with("http://") || trimmed.to_ascii_lowercase().starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    // Avoids a doubled "//index.php".
    let base = with_scheme.trim_end_matches('/').to_string();
    let start_url = format!("{base}/index.php/login/v2");

    let response = reqwest::Client::new()
        .post(&start_url)
        .header("User-Agent", "Phodder")
        .send()
        .await
        .map_err(|e| LoginFlowError::StartRequestFailed(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(LoginFlowError::UnexpectedStartResponse(format!("HTTP {status}")));
    }

    let parsed: StartResponse = response.json().await.map_err(|e| LoginFlowError::UnexpectedStartResponse(e.to_string()))?;

    Ok(LoginFlowStart { login_url: parsed.login, poll_token: parsed.poll.token, poll_endpoint: parsed.poll.endpoint })
}

/// Polls until the user finishes in the browser, TOKEN_LIFETIME passes, or
/// `cancelled` is set. Cancellation is checked before the first poll. The
/// caller must already have opened `flow.login_url`.
pub async fn poll_until_complete(flow: &LoginFlowStart, cancelled: &std::sync::Arc<std::sync::atomic::AtomicBool>) -> Result<LoginFlowResult, LoginFlowError> {
    let deadline = std::time::Instant::now() + TOKEN_LIFETIME;
    loop {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(LoginFlowError::Cancelled);
        }
        if std::time::Instant::now() >= deadline {
            return Err(LoginFlowError::TimedOut);
        }

        let response = reqwest::Client::new().post(&flow.poll_endpoint).form(&[("token", flow.poll_token.as_str())]).send().await;

        match response {
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                // 404: still waiting.
            }
            Ok(response) if response.status().is_success() => {
                match response.json::<PollSuccessResponse>().await {
                    Ok(result) => {
                        return Ok(LoginFlowResult {
                            server: result.server,
                            login_name: result.login_name,
                            app_password: result.app_password,
                        });
                    }
                    // A malformed 200 ends the flow instead of retrying
                    // forever.
                    Err(_) => return Err(LoginFlowError::TimedOut),
                }
            }
            // Other statuses and transient network errors keep polling; the
            // next poll may succeed.
            _ => {}
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Revokes the app password server-side (`DELETE
/// <server>/ocs/v2.php/core/apppassword`, authenticated with that same
/// password), per Login Flow v2's guidance for removed accounts. Best-effort:
/// the docs say to remove the account even when this fails.
pub async fn revoke_app_password(server: &str, login_name: &str, app_password: &str) -> Result<(), String> {
    let url = format!("{}/ocs/v2.php/core/apppassword", server.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .delete(&url)
        .basic_auth(login_name, Some(app_password))
        .header("OCS-APIREQUEST", "true")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {status}"))
    }
}
