//! NetworkManager queries on the system bus: connection type for the download
//! concurrency default (4 on WiFi/ethernet, 2 on cellular) and metered status
//! for gating automatic refresh and downloads.
//!
//! Flatpak packaging needs
//! `--system-talk-name=org.freedesktop.NetworkManager`.

const CONCURRENCY_WIFI_OR_ETHERNET: usize = 4;
const CONCURRENCY_CELLULAR: usize = 2;

/// Gates automatic refresh and downloads; only user-started actions bypass
/// it. Detection failure reports not metered.
pub async fn is_metered_connection() -> bool {
    match query_metered_property().await {
        Ok(metered) => metered,
        Err(e) => {
            tracing::warn!("failed to query NetworkManager metered status, assuming not metered: {e}");
            false
        }
    }
}

/// NetworkManager's `Metered` is a u32 enum: 0 unknown, 1 yes, 2 no, 3
/// guess-yes, 4 guess-no. Yes and guess-yes count as metered; unknown
/// doesn't.
async fn query_metered_property() -> zbus::Result<bool> {
    let connection = zbus::Connection::system().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )
    .await?;
    let metered: u32 = proxy.get_property("Metered").await?;
    Ok(matches!(metered, 1 | 3))
}

/// Detection failure (no NetworkManager, no active connection, a D-Bus error)
/// gives the WiFi limit: a wrong generous default costs less than wrongly
/// throttling.
pub async fn detect_max_concurrent_downloads() -> usize {
    match query_primary_connection_type().await {
        Ok(Some(conn_type)) if is_cellular(&conn_type) => {
            tracing::debug!("detected cellular connection ({conn_type}), using concurrency {CONCURRENCY_CELLULAR}");
            CONCURRENCY_CELLULAR
        }
        Ok(Some(conn_type)) => {
            tracing::debug!("detected non-cellular connection ({conn_type}), using concurrency {CONCURRENCY_WIFI_OR_ETHERNET}");
            CONCURRENCY_WIFI_OR_ETHERNET
        }
        Ok(None) => {
            tracing::debug!("no active NetworkManager connection detected, defaulting to WiFi-level concurrency");
            CONCURRENCY_WIFI_OR_ETHERNET
        }
        Err(e) => {
            tracing::warn!(
                "failed to query NetworkManager connection type, defaulting to WiFi-level concurrency: {e}"
            );
            CONCURRENCY_WIFI_OR_ETHERNET
        }
    }
}

/// `PrimaryConnectionType` uses NM's device-type setting names: "gsm" and
/// "cdma" for mobile broadband.
fn is_cellular(conn_type: &str) -> bool {
    matches!(conn_type, "gsm" | "cdma" | "wimax")
}

/// `Ok(None)` when there is no active connection (NM reports an empty
/// string).
async fn query_primary_connection_type() -> zbus::Result<Option<String>> {
    let connection = zbus::Connection::system().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )
    .await?;
    let conn_type: String = proxy.get_property("PrimaryConnectionType").await?;
    if conn_type.is_empty() {
        Ok(None)
    } else {
        Ok(Some(conn_type))
    }
}
