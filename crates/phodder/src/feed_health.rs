//! In-memory feed refresh health and network availability, reported to
//! the UI as one `UiUpdate::RefreshStatus`. Kept in memory: the project
//! has no schema migrations, and startup plus the periodic pass rebuild
//! it quickly.

use crate::i18n::{gettext, gettext_f};

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Background failures are reported once a feed has failed this many
/// times in a row, or for `REPORT_AFTER_DURATION`. A single blip stays quiet.
const REPORT_AFTER_CONSECUTIVE: u32 = 2;
const REPORT_AFTER_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FeedProblem {
    pub subscription_id: i64,
    pub title: String,
    pub reason: String,
}

struct Failure {
    title: String,
    reason: String,
    consecutive: u32,
    since: Instant,
    /// Set by a user-started refresh: report without waiting for the threshold.
    report_now: bool,
}

static FAILURES: LazyLock<Mutex<HashMap<i64, Failure>>> = LazyLock::new(Default::default);
const NETWORK_UNKNOWN: u8 = 0;
const NETWORK_UP: u8 = 1;
const NETWORK_DOWN: u8 = 2;
/// Reported by the service's GNetworkMonitor; unknown until its first report.
static NETWORK: AtomicU8 = AtomicU8::new(NETWORK_UNKNOWN);
static NETWORK_REPORTED: tokio::sync::Notify = tokio::sync::Notify::const_new();
/// The startup refresh waits this long for the first report, then assumes
/// the network is up. Longer than the service's grace for an "unavailable"
/// startup reading, so a real offline start skips the refresh.
const FIRST_REPORT_TIMEOUT: Duration = Duration::from_secs(6);

pub fn network_available() -> bool {
    NETWORK.load(Ordering::Relaxed) != NETWORK_DOWN
}

pub async fn wait_for_network_state() {
    let reported = NETWORK_REPORTED.notified();
    if NETWORK.load(Ordering::Relaxed) != NETWORK_UNKNOWN {
        return;
    }
    let _ = tokio::time::timeout(FIRST_REPORT_TIMEOUT, reported).await;
}

/// Returns true when the network has just come back.
pub fn set_network_available(available: bool) -> bool {
    let previous = NETWORK.swap(if available { NETWORK_UP } else { NETWORK_DOWN }, Ordering::Relaxed);
    NETWORK_REPORTED.notify_waiters();
    available && previous == NETWORK_DOWN
}

pub fn record_success(subscription_id: i64) {
    FAILURES.lock().unwrap().remove(&subscription_id);
}

pub fn record_failure(subscription_id: i64, title: &str, error: &anyhow::Error, user_initiated: bool) {
    let reason = friendly_reason(error);
    let mut failures = FAILURES.lock().unwrap();
    let entry = failures.entry(subscription_id).or_insert_with(|| Failure {
        title: title.to_string(),
        reason: reason.clone(),
        consecutive: 0,
        since: Instant::now(),
        report_now: false,
    });
    entry.title = title.to_string();
    entry.reason = reason;
    entry.consecutive += 1;
    entry.report_now |= user_initiated;
}

/// Drops failures for subscriptions that no longer exist.
pub fn retain_only(subscription_ids: &[i64]) {
    FAILURES.lock().unwrap().retain(|id, _| subscription_ids.contains(id));
}

/// Failures worth showing, ordered by title.
pub fn reportable() -> Vec<FeedProblem> {
    let failures = FAILURES.lock().unwrap();
    let mut problems: Vec<FeedProblem> = failures
        .iter()
        .filter(|(_, f)| f.report_now || f.consecutive >= REPORT_AFTER_CONSECUTIVE || f.since.elapsed() >= REPORT_AFTER_DURATION)
        .map(|(id, f)| FeedProblem { subscription_id: *id, title: f.title.clone(), reason: f.reason.clone() })
        .collect();
    problems.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    problems
}

pub async fn send_status(ui_tx: &async_channel::Sender<crate::ui::UiUpdate>) {
    let update = crate::ui::UiUpdate::RefreshStatus { offline: !network_available(), feed_problems: reportable() };
    if let Err(e) = ui_tx.send(update).await {
        tracing::debug!("no UI listening for refresh status: {e}");
    }
}

/// Short, human-readable cause for a refresh failure.
pub fn friendly_reason(error: &anyhow::Error) -> String {
    for cause in error.chain() {
        if let Some(e) = cause.downcast_ref::<reqwest::Error>() {
            if e.is_timeout() {
                return gettext("Timed out");
            }
            if let Some(status) = e.status() {
                let code = status.as_u16().to_string();
                let args = [("code", code.as_str())];
                return match status.as_u16() {
                    404 | 410 => gettext_f("Feed not found ({code})", &args),
                    401 | 403 => gettext_f("Access denied ({code})", &args),
                    s if s >= 500 => gettext_f("Server error ({code})", &args),
                    _ => gettext_f("HTTP error {code}", &args),
                };
            }
            if e.is_connect() || e.is_request() {
                return gettext("Couldn't connect to the server");
            }
            if e.is_decode() || e.is_body() {
                return gettext("Connection interrupted");
            }
        }
    }
    let text = error.to_string();
    let lower = text.to_lowercase();
    if lower.contains("parse") || lower.contains("xml") || lower.contains("rss") {
        return gettext("Couldn't read the feed");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_failures_report_after_two_in_a_row_and_user_failures_at_once() {
        let err = anyhow::anyhow!("boom");
        record_failure(9001, "Quiet", &err, false);
        assert!(!reportable().iter().any(|p| p.subscription_id == 9001));
        record_failure(9001, "Quiet", &err, false);
        assert!(reportable().iter().any(|p| p.subscription_id == 9001));
        record_success(9001);
        assert!(!reportable().iter().any(|p| p.subscription_id == 9001));

        record_failure(9002, "Loud", &err, true);
        assert!(reportable().iter().any(|p| p.subscription_id == 9002));
        record_success(9002);
    }

    #[test]
    fn network_comes_back_once() {
        assert!(!set_network_available(true));
        assert!(!set_network_available(false));
        assert!(set_network_available(true));
        assert!(!set_network_available(true));
    }
}
