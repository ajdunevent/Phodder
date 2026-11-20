//! Podcast search and popular lists from directories, each switched on or
//! off in Settings: Apple's iTunes Search API and gpodder.net, which need no
//! key, and Podcast Index, which uses the user's own key and secret. Results
//! from several sources are merged by feed URL.

use std::time::Duration;

use serde::Deserialize;

/// Settings keys; both sources default to on.
pub const APPLE_SETTING: &str = "discovery_apple_enabled";
pub const GPODDER_SETTING: &str = "discovery_gpodder_enabled";
/// Podcast Index is used when this key is set; the secret is in the keyring
/// under PODCAST_INDEX_SECRET_REF.
pub const PODCAST_INDEX_KEY_SETTING: &str = "podcastindex_api_key";
/// Lets saved credentials stay while Podcast Index is switched off.
pub const PODCAST_INDEX_SETTING: &str = "discovery_podcastindex_enabled";
pub const PODCAST_INDEX_SECRET_REF: &str = "podcastindex:api-secret";

const RESULT_LIMIT: usize = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Apple,
    Gpodder,
    PodcastIndex,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Apple => "Apple Podcasts",
            Source::Gpodder => "gpodder.net",
            Source::PodcastIndex => "Podcast Index",
        }
    }
}

/// Merged results, plus the sources that failed while others answered.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryResults {
    pub podcasts: Vec<DiscoveredPodcast>,
    pub failed: Vec<Source>,
    /// Podcast Index refused the user's key and secret.
    pub key_rejected: bool,
}

#[derive(Debug)]
struct KeyRejected;

impl std::fmt::Display for KeyRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Podcast Index rejected the key and secret")
    }
}

impl std::error::Error for KeyRejected {}

#[derive(Debug, Clone)]
pub struct DiscoveredPodcast {
    pub title: String,
    pub author: Option<String>,
    pub feed_url: String,
    pub artwork_url: Option<String>,
    pub source: Source,
}

/// The sources switched on in Settings, in display order.
pub fn enabled_sources(conn: &rusqlite::Connection) -> Vec<Source> {
    let on = |key| phodder_core::settings::get_setting(conn, key).map(|v| v != "0").unwrap_or(true);
    let mut sources = Vec::new();
    if on(APPLE_SETTING) {
        sources.push(Source::Apple);
    }
    if on(GPODDER_SETTING) {
        sources.push(Source::Gpodder);
    }
    if on(PODCAST_INDEX_SETTING) && podcast_index_key(conn).is_some() {
        sources.push(Source::PodcastIndex);
    }
    sources
}

pub fn podcast_index_key(conn: &rusqlite::Connection) -> Option<String> {
    phodder_core::settings::get_setting(conn, PODCAST_INDEX_KEY_SETTING).filter(|k| !k.trim().is_empty())
}

/// What a Podcast Index request is signed with.
#[derive(Clone)]
pub struct PodcastIndexCredentials {
    pub key: String,
    pub secret: zeroize::Zeroizing<String>,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        // The page waits for every source, so a slow one holds up the rest.
        .timeout(Duration::from_secs(10))
        .user_agent(crate::USER_AGENT)
        .build()
        .unwrap_or_default()
}

/// The locale's language ("de_DE.UTF-8" -> "de"), for Podcast Index's
/// trending list; None for the C/POSIX locale.
fn locale_language() -> Option<String> {
    std::env::var("LC_ALL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("LANG").ok())
        .and_then(|locale| locale.split(['_', '.']).next().map(str::to_lowercase))
        .filter(|language| language.len() == 2 && language.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Apple's storefront from the locale ("en_GB.UTF-8" -> "gb"), else "us".
fn apple_country() -> String {
    std::env::var("LC_ALL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("LANG").ok())
        .and_then(|locale| locale.split(['_', '.']).nth(1).map(str::to_lowercase))
        .filter(|country| country.len() == 2 && country.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or_else(|| "us".to_string())
}

/// Searches every source at once. A failing source is logged and left out,
/// so one being down doesn't hide the others' results; the error is only
/// returned when every source failed.
pub async fn search(sources: &[Source], podcast_index: Option<&PodcastIndexCredentials>, term: &str) -> anyhow::Result<DiscoveryResults> {
    let client = client();
    let results = futures_util::future::join_all(sources.iter().map(|source| {
        let client = client.clone();
        async move {
            match source {
                Source::Apple => apple_search(&client, term).await,
                Source::Gpodder => gpodder_search(&client, term).await,
                Source::PodcastIndex => podcast_index_request(&client, podcast_index, "search/byterm", &[("q", term)]).await,
            }
        }
    }))
    .await;
    merge(sources, results)
}

/// Charts change slowly, and Apple limits each address to about 20
/// requests a minute, so a popular list is reused for an hour.
const POPULAR_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

static POPULAR_CACHE: std::sync::Mutex<Option<(Vec<Source>, std::time::Instant, DiscoveryResults)>> = std::sync::Mutex::new(None);

/// Each source's popular list, merged the same way as search results.
pub async fn popular(sources: &[Source], podcast_index: Option<&PodcastIndexCredentials>) -> anyhow::Result<DiscoveryResults> {
    if let Some((cached_sources, at, results)) = POPULAR_CACHE.lock().unwrap().as_ref() {
        if cached_sources == sources && at.elapsed() < POPULAR_CACHE_TTL && results.failed.is_empty() {
            return Ok(results.clone());
        }
    }
    let client = client();
    let results = futures_util::future::join_all(sources.iter().map(|source| {
        let client = client.clone();
        async move {
            match source {
                Source::Apple => apple_top(&client).await,
                Source::Gpodder => gpodder_top(&client).await,
                Source::PodcastIndex => {
                    let language = locale_language();
                    let mut query = vec![];
                    if let Some(language) = language.as_deref() {
                        query.push(("lang", language));
                    }
                    podcast_index_request(&client, podcast_index, "podcasts/trending", &query).await
                }
            }
        }
    }))
    .await;
    let merged = merge(sources, results)?;
    *POPULAR_CACHE.lock().unwrap() = Some((sources.to_vec(), std::time::Instant::now(), merged.clone()));
    Ok(merged)
}

fn merge(sources: &[Source], results: Vec<anyhow::Result<Vec<DiscoveredPodcast>>>) -> anyhow::Result<DiscoveryResults> {
    let mut merged: Vec<DiscoveredPodcast> = Vec::new();
    let mut failed = Vec::new();
    let mut key_rejected = false;
    let mut seen = std::collections::HashSet::new();
    let mut last_error = None;
    let mut any_ok = false;
    for (source, result) in sources.iter().zip(results) {
        match result {
            Ok(podcasts) => {
                any_ok = true;
                for podcast in podcasts {
                    if seen.insert(normalize_feed_url(&podcast.feed_url)) {
                        merged.push(podcast);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("{} discovery request failed: {e:#}", source.label());
                failed.push(*source);
                key_rejected |= e.is::<KeyRejected>();
                last_error = Some(e);
            }
        }
    }
    match (any_ok, last_error) {
        (false, Some(e)) => Err(e),
        _ => Ok(DiscoveryResults { podcasts: merged, failed, key_rejected }),
    }
}

/// Feeds listed under both http and https, or with a trailing slash, are
/// the same feed.
pub fn normalize_feed_url(url: &str) -> String {
    let lower = url.trim().to_lowercase();
    let without_scheme = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")).unwrap_or(&lower);
    without_scheme.trim_end_matches('/').to_string()
}

#[derive(Deserialize)]
struct AppleResults {
    results: Vec<AppleResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppleResult {
    collection_id: Option<u64>,
    collection_name: Option<String>,
    artist_name: Option<String>,
    feed_url: Option<String>,
    artwork_url600: Option<String>,
    artwork_url100: Option<String>,
}

impl AppleResult {
    /// Entries without a feed URL (Apple-exclusive shows) can't be
    /// subscribed to and are dropped.
    fn into_podcast(self) -> Option<DiscoveredPodcast> {
        Some(DiscoveredPodcast {
            title: self.collection_name?,
            author: self.artist_name,
            feed_url: self.feed_url.filter(|u| !u.is_empty())?,
            artwork_url: self.artwork_url600.or(self.artwork_url100),
            source: Source::Apple,
        })
    }
}

async fn apple_search(client: &reqwest::Client, term: &str) -> anyhow::Result<Vec<DiscoveredPodcast>> {
    let response: AppleResults = client
        .get("https://itunes.apple.com/search")
        .query(&[("media", "podcast"), ("entity", "podcast"), ("term", term), ("country", &apple_country()), ("limit", &RESULT_LIMIT.to_string())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(response.results.into_iter().filter_map(AppleResult::into_podcast).collect())
}

#[derive(Deserialize)]
struct AppleChart {
    feed: AppleChartFeed,
}

#[derive(Deserialize)]
struct AppleChartFeed {
    results: Vec<AppleChartEntry>,
}

#[derive(Deserialize)]
struct AppleChartEntry {
    id: String,
}

/// The chart has no feed URLs, so its ids go through one batched lookup.
/// Apple documents two hosts for the chart; the second is tried if the
/// first fails.
async fn apple_top(client: &reqwest::Client) -> anyhow::Result<Vec<DiscoveredPodcast>> {
    let country = apple_country();
    let mut chart = None;
    let mut last_error = None;
    for host in ["rss.marketingtools.apple.com", "rss.applemarketingtools.com"] {
        let url = format!("https://{host}/api/v2/{country}/podcasts/top/{RESULT_LIMIT}/podcasts.json");
        match async { anyhow::Ok(client.get(&url).send().await?.error_for_status()?.json::<AppleChart>().await?) }.await {
            Ok(c) => {
                chart = Some(c);
                break;
            }
            Err(e) => last_error = Some(e),
        }
    }
    let chart = match chart {
        Some(chart) => chart,
        None => return Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no Apple chart host answered"))),
    };
    let ids: Vec<String> = chart.feed.results.into_iter().map(|e| e.id).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let lookup: AppleResults = client
        .get("https://itunes.apple.com/lookup")
        .query(&[("id", ids.join(",").as_str()), ("entity", "podcast"), ("country", &country)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    // Lookup doesn't promise chart order, so results are put back in it.
    let rank = |result: &AppleResult| result.collection_id.and_then(|id| ids.iter().position(|chart_id| *chart_id == id.to_string())).unwrap_or(usize::MAX);
    let mut results = lookup.results;
    results.sort_by_key(|result| rank(result));
    Ok(results.into_iter().filter_map(AppleResult::into_podcast).collect())
}

#[derive(Deserialize)]
struct GpodderPodcast {
    url: String,
    title: Option<String>,
    author: Option<String>,
    logo_url: Option<String>,
}

impl GpodderPodcast {
    fn into_podcast(self) -> Option<DiscoveredPodcast> {
        let title = self.title.filter(|t| !t.trim().is_empty())?;
        Some(DiscoveredPodcast {
            title,
            author: self.author.filter(|a| !a.trim().is_empty()),
            feed_url: self.url,
            artwork_url: self.logo_url.filter(|u| !u.is_empty()),
            source: Source::Gpodder,
        })
    }
}

async fn gpodder_search(client: &reqwest::Client, term: &str) -> anyhow::Result<Vec<DiscoveredPodcast>> {
    let podcasts: Vec<GpodderPodcast> =
        client.get("https://gpodder.net/search.json").query(&[("q", term)]).send().await?.error_for_status()?.json().await?;
    Ok(podcasts.into_iter().filter_map(GpodderPodcast::into_podcast).take(RESULT_LIMIT).collect())
}

async fn gpodder_top(client: &reqwest::Client) -> anyhow::Result<Vec<DiscoveredPodcast>> {
    let podcasts: Vec<GpodderPodcast> =
        client.get(format!("https://gpodder.net/toplist/{RESULT_LIMIT}.json")).send().await?.error_for_status()?.json().await?;
    Ok(podcasts.into_iter().filter_map(GpodderPodcast::into_podcast).collect())
}

/// Podcast Index signs each request: Authorization is the hex SHA-1 of
/// key + secret + X-Auth-Date, and the date must be within a few minutes of
/// the server's clock.
fn podcast_index_authorization(key: &str, secret: &str, unix_time: u64) -> String {
    use sha1::Digest;
    let digest = sha1::Sha1::digest(format!("{key}{secret}{unix_time}").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Deserialize)]
struct PodcastIndexFeeds {
    #[serde(default)]
    feeds: Vec<PodcastIndexFeed>,
}

#[derive(Deserialize)]
struct PodcastIndexFeed {
    title: Option<String>,
    author: Option<String>,
    url: Option<String>,
    artwork: Option<String>,
    image: Option<String>,
}

impl PodcastIndexFeed {
    fn into_podcast(self) -> Option<DiscoveredPodcast> {
        Some(DiscoveredPodcast {
            title: self.title.filter(|t| !t.trim().is_empty())?,
            author: self.author.filter(|a| !a.trim().is_empty()),
            feed_url: self.url.filter(|u| !u.is_empty())?,
            artwork_url: self.artwork.filter(|u| !u.is_empty()).or(self.image.filter(|u| !u.is_empty())),
            source: Source::PodcastIndex,
        })
    }
}

async fn podcast_index_request(
    client: &reqwest::Client,
    credentials: Option<&PodcastIndexCredentials>,
    endpoint: &str,
    query: &[(&str, &str)],
) -> anyhow::Result<Vec<DiscoveredPodcast>> {
    let credentials = credentials.ok_or_else(|| anyhow::anyhow!("the Podcast Index secret is missing from the keyring"))?;
    let unix_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let max = RESULT_LIMIT.to_string();
    let response = client
        .get(format!("https://api.podcastindex.org/api/1.0/{endpoint}"))
        .query(query)
        .query(&[("max", max.as_str())])
        .header("X-Auth-Key", &credentials.key)
        .header("X-Auth-Date", unix_time.to_string())
        .header("Authorization", podcast_index_authorization(&credentials.key, &credentials.secret, unix_time))
        .send()
        .await?;
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(KeyRejected.into());
    }
    let feeds: PodcastIndexFeeds = response.error_for_status()?.json().await?;
    Ok(feeds.feeds.into_iter().filter_map(PodcastIndexFeed::into_podcast).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_entries_without_a_feed_are_dropped() {
        let json = r#"{"results":[
            {"collectionName":"Open Show","artistName":"A","feedUrl":"https://example.com/a.xml","artworkUrl600":"https://example.com/a.jpg"},
            {"collectionName":"Apple Exclusive","artistName":"B"}
        ]}"#;
        let results: AppleResults = serde_json::from_str(json).unwrap();
        let podcasts: Vec<_> = results.results.into_iter().filter_map(AppleResult::into_podcast).collect();
        assert_eq!(podcasts.len(), 1);
        assert_eq!(podcasts[0].feed_url, "https://example.com/a.xml");
        assert_eq!(podcasts[0].artwork_url.as_deref(), Some("https://example.com/a.jpg"));
    }

    #[test]
    fn gpodder_entries_parse_and_untitled_ones_are_dropped() {
        let json = r#"[
            {"url":"http://example.com/feed","title":"Show","author":"","logo_url":"http://example.com/l.png","subscribers":12},
            {"url":"http://example.com/other","title":"  "}
        ]"#;
        let podcasts: Vec<GpodderPodcast> = serde_json::from_str(json).unwrap();
        let podcasts: Vec<_> = podcasts.into_iter().filter_map(GpodderPodcast::into_podcast).collect();
        assert_eq!(podcasts.len(), 1);
        assert_eq!(podcasts[0].author, None);
    }

    #[test]
    fn merging_drops_a_feed_already_listed_by_an_earlier_source() {
        let podcast = |url: &str, source| DiscoveredPodcast { title: "T".into(), author: None, feed_url: url.into(), artwork_url: None, source };
        let merged = merge(
            &[Source::Apple, Source::Gpodder],
            vec![
                Ok(vec![podcast("https://example.com/feed/", Source::Apple)]),
                Ok(vec![podcast("http://example.com/feed", Source::Gpodder), podcast("http://example.com/b", Source::Gpodder)]),
            ],
        )
        .unwrap()
        .podcasts;
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].source, Source::Apple);
    }

    #[test]
    fn podcast_index_signature_matches_its_documented_recipe() {
        // sha1("keysecret1700000000"), computed independently with Python.
        assert_eq!(podcast_index_authorization("key", "secret", 1_700_000_000), "abaf71c02050c31e4d4e6b08c1625173af0445ba");
    }

    #[test]
    fn podcast_index_feeds_parse_and_prefer_artwork_over_image() {
        let json = r#"{"status":"true","feeds":[
            {"id":1,"title":"Show","author":"A","url":"https://example.com/feed","image":"https://example.com/i.jpg","artwork":"https://example.com/a.jpg"},
            {"id":2,"title":"No Feed","url":""}
        ],"count":2}"#;
        let feeds: PodcastIndexFeeds = serde_json::from_str(json).unwrap();
        let podcasts: Vec<_> = feeds.feeds.into_iter().filter_map(PodcastIndexFeed::into_podcast).collect();
        assert_eq!(podcasts.len(), 1);
        assert_eq!(podcasts[0].artwork_url.as_deref(), Some("https://example.com/a.jpg"));
    }

    #[test]
    fn one_failing_source_still_returns_the_others() {
        let merged = merge(&[Source::Apple, Source::Gpodder], vec![Err(anyhow::anyhow!("down")), Ok(vec![])]).unwrap();
        assert!(merged.podcasts.is_empty());
        assert_eq!(merged.failed, vec![Source::Apple]);
        assert!(merge(&[Source::Apple], vec![Err(anyhow::anyhow!("down"))]).is_err());
    }
}

