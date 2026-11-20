//! gpodder-API wire shapes shared by gpodder.net and Nextcloud's GPodder Sync
//! app, which replicates them (github.com/thrillfall/nextcloud-gpodder). The
//! backends differ only in URLs and authentication, which live in the binary
//! crate's gpodder_sync.rs. Reference: gpoddernet.readthedocs.io.

use serde::{Deserialize, Serialize};

/// One episode action. The episode guid is always sent when known: `guid` is
/// optional in the API, and the media URL alone can change between devices
/// through redirects or sanitizing.
///
/// `started`, `position` and `total` apply to "play" only and are omitted
/// otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpisodeActionRecord {
    /// The feed URL, the only identifier shared across apps.
    pub podcast: String,
    /// The media (enclosure) URL.
    pub episode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
    /// Sent lowercase, as in gpodder.net's examples; Nextcloud's examples
    /// show "PLAY", so incoming values are matched case-insensitively
    /// (`action_kind`).
    pub action: String,
    /// ISO 8601 without an offset ("2009-12-12T09:00:00") in both APIs'
    /// examples. Servers send offsets anyway; see remote_action_is_newer in
    /// the binary crate.
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
    /// The device that did it. gpodder.net records it; Nextcloud ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// `action` parsed case-insensitively. `Other` keeps unrecognized kinds
/// instead of dropping them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Download,
    Play,
    Delete,
    New,
}

impl EpisodeActionRecord {
    pub fn action_kind(&self) -> Option<ActionKind> {
        match self.action.to_ascii_lowercase().as_str() {
            "download" => Some(ActionKind::Download),
            "play" => Some(ActionKind::Play),
            "delete" => Some(ActionKind::Delete),
            "new" => Some(ActionKind::New),
            _ => None,
        }
    }
}

/// GET episode actions: `{"actions": [...], "timestamp": ...}` for both
/// backends.
#[derive(Debug, Clone, Deserialize)]
pub struct EpisodeActionsResponse {
    pub actions: Vec<EpisodeActionRecord>,
    pub timestamp: i64,
}

/// POST episode actions. `update_urls` (URL rewrites) is gpodder.net-only;
/// `#[serde(default)]` accepts Nextcloud responses without it.
#[derive(Debug, Clone, Deserialize)]
pub struct EpisodeActionsUploadResponse {
    pub timestamp: i64,
    #[serde(default)]
    pub update_urls: Vec<(String, String)>,
}

/// Plain feed URLs; titles and artwork come from fetching the feed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SubscriptionChangeRequest {
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

/// `update_urls` is gpodder.net-only, as in EpisodeActionsUploadResponse.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SubscriptionChangeResponse {
    // Nextcloud documents POST /subscription_change/create only as "returns
    // JSON with current timestamp", and a timestamp-only body failed with
    // "error decoding response body". Defaults let it decode.
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
    pub timestamp: i64,
    #[serde(default)]
    pub update_urls: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_action_omits_play_only_fields_when_absent() {
        let record = EpisodeActionRecord {
            podcast: "https://example.com/feed.rss".to_string(),
            episode: "https://example.com/ep.mp3".to_string(),
            guid: Some("abc123".to_string()),
            action: "download".to_string(),
            timestamp: "2009-12-12T09:00:00".to_string(),
            started: None,
            position: None,
            total: None,
            device: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("started"));
        assert!(!json.contains("position"));
        assert!(!json.contains("total"));
    }

    #[test]
    fn parses_a_real_nextcloud_episode_actions_response() {
        // Nextcloud's README example response.
        let json = r#"{
            "actions": [
              {
               "podcast": "http://example.com/feed.rss",
               "episode": "http://example.com/files/s01e20.mp3",
               "guid": "s01e20-example-org",
               "action": "PLAY",
               "timestamp": "2009-12-12T09:00:00",
               "started": 15,
               "position": 120,
               "total": 500
              },
              {
               "podcast": "http://example.com/feed.rss",
               "episode": "http://example.com/files/s01e20.mp3",
               "guid": "s01e20-example-org",
               "action": "DOWNLOAD",
               "timestamp": "2009-12-12T09:00:00",
               "started": -1,
               "position": -1,
               "total": -1
              }
            ],
            "timestamp": 12345
        }"#;
        let parsed: EpisodeActionsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.timestamp, 12345);
        assert_eq!(parsed.actions.len(), 2);
        assert_eq!(parsed.actions[0].action_kind(), Some(ActionKind::Play));
        assert_eq!(parsed.actions[1].action_kind(), Some(ActionKind::Download));
    }

    #[test]
    fn action_kind_matches_case_insensitively() {
        let mut record = EpisodeActionRecord {
            podcast: String::new(),
            episode: String::new(),
            guid: None,
            action: "PLAY".to_string(),
            timestamp: String::new(),
            started: None,
            position: None,
            total: None,
            device: None,
        };
        assert_eq!(record.action_kind(), Some(ActionKind::Play));
        record.action = "play".to_string();
        assert_eq!(record.action_kind(), Some(ActionKind::Play));
        record.action = "Play".to_string();
        assert_eq!(record.action_kind(), Some(ActionKind::Play));
    }

    #[test]
    fn unrecognized_action_kind_is_none_not_an_error() {
        let record = EpisodeActionRecord {
            podcast: String::new(),
            episode: String::new(),
            guid: None,
            action: "some-future-action-this-app-does-not-know-about".to_string(),
            timestamp: String::new(),
            started: None,
            position: None,
            total: None,
            device: None,
        };
        assert_eq!(record.action_kind(), None);
    }

    #[test]
    fn subscription_change_response_defaults_update_urls_when_absent() {
        // No update_urls in a Nextcloud response.
        let json = r#"{"add": ["https://example.com/feed.xml"], "remove": [], "timestamp": 1663540502}"#;
        let parsed: SubscriptionChangeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.add, vec!["https://example.com/feed.xml".to_string()]);
        assert_eq!(parsed.timestamp, 1663540502);
        assert_eq!(parsed.update_urls, Vec::<(String, String)>::new());
    }

    #[test]
    fn subscription_change_response_defaults_add_and_remove_when_absent() {
        // A timestamp-only body, as Nextcloud's subscription_change endpoint
        // can return.
        let json = r#"{"timestamp": 1663540502}"#;
        let parsed: SubscriptionChangeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.add, Vec::<String>::new());
        assert_eq!(parsed.remove, Vec::<String>::new());
        assert_eq!(parsed.timestamp, 1663540502);
    }

    #[test]
    fn subscription_change_response_parses_gpodder_net_update_urls_when_present() {
        let json = r#"{"add": [], "remove": [], "timestamp": 1, "update_urls": [["http://old.example.com/feed", "https://new.example.com/feed"]]}"#;
        let parsed: SubscriptionChangeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.update_urls, vec![("http://old.example.com/feed".to_string(), "https://new.example.com/feed".to_string())]);
    }
}
