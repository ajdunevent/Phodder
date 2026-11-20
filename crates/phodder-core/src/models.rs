//! Domain models mirroring schema.sql. Rows are mapped by hand in the query
//! modules.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: i64,
    pub name: String,
    /// Fractional index; see schema.sql.
    pub sort_order: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadStatus {
    None,
    Queued,
    Downloading,
    Downloaded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: i64,
    pub feed_url: String,
    pub title: String,
    /// The feed's current title, updated on every refresh separately from
    /// `title` (see schema.sql).
    pub feed_title: Option<String>,
    pub artwork_url: Option<String>,
    /// The downloaded thumbnail the UI loads; `None` until downloaded.
    pub artwork_local_path: Option<String>,
    /// The show's channel description, not episode show notes.
    pub description: Option<String>,
    pub group_id: i64,
    pub auto_download_enabled: bool,
    pub notify_on_new_episodes: bool,
    /// Parsed from the `download_filter_rules` JSON column (see filter.rs).
    pub download_filter_rules: Vec<crate::filter::FilterRule>,
    pub chapter_skip_rules: Vec<crate::filter::FilterRule>,
    /// Expiry in minutes after publication (retention.rs), so short windows
    /// such as 5 minutes work. Every subscription has its own value; there's
    /// no global default.
    pub delete_after_minutes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: i64,
    pub subscription_id: i64,
    pub guid: String,
    pub title: String,
    pub audio_url: String,
    pub duration_ms: Option<i64>,
    pub publish_date: DateTime<Utc>,

    pub download_status: DownloadStatus,
    pub retry_count: i64,
    pub last_error_reason: Option<String>,
    pub local_file_path: Option<String>,
    pub file_size_bytes: Option<i64>,

    pub played: bool,
    pub playback_position_ms: i64,

    /// `None` when not queued; otherwise a fractional position within the
    /// subscription's group (see queue.rs).
    pub group_queue_position: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chapter {
    pub id: i64,
    pub episode_id: i64,
    pub start_ms: i64,
    pub title: Option<String>,
}
