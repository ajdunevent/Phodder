//! Feed refresh (parsing untrusted XML) and episode downloading, run as a
//! Tokio task: a panic here resolves the task's JoinHandle with Err and
//! leaves playback running.
//!
//! Covers the refresh schedule (gated on metered connections), conditional
//! GETs, a shared feed cache, downloads with a disk-space preflight, a
//! concurrency limit from the network type (network.rs), retry with backoff,
//! and a completion broadcast for the coordinator's retention sweep.
//! Credentials for private feeds aren't supported.

use futures_util::StreamExt;
use rusqlite::OptionalExtension;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Semaphore};

pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

fn http_client_for_download() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(600))
        .build()
        .unwrap_or_default()
}

#[derive(Debug)]
struct EpisodeGoneFromFeed;
impl std::fmt::Display for EpisodeGoneFromFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "episode no longer present in its subscription's feed at all -- likely retracted or aged out of the feed's own retention window")
    }
}
impl std::error::Error for EpisodeGoneFromFeed {}

fn episode_qualifies_for_auto_download(
    episode_label: &str,
    auto_download_enabled: bool,
    delete_after_minutes: i64,
    compiled_filter_rules: Option<&[phodder_core::filter::CompiledFilterRule]>,
    publish_date: chrono::DateTime<chrono::Utc>,
    title: &str,
    show_notes_text: &str,
    played: bool,
) -> bool {
    if !auto_download_enabled {
        tracing::debug!("episode {episode_label}: not downloading, automatic downloads are off for its podcast");
        return false;
    }
    if played {
        tracing::debug!("episode {episode_label}: not downloading, already played");
        return false;
    }
    let expiry_cutoff = chrono::Utc::now() - chrono::Duration::minutes(delete_after_minutes);
    if publish_date < expiry_cutoff {
        tracing::debug!("episode {episode_label}: not downloading, published {publish_date}, before its podcast's expiry cutoff {expiry_cutoff}");
        return false;
    }
    match compiled_filter_rules {
        Some(compiled) => {
            let decision = phodder_core::filter::evaluate(compiled, title, show_notes_text);
            if decision != phodder_core::filter::FilterDecision::Allow {
                tracing::debug!("episode {episode_label}: not downloading, blocked by its podcast's filters (title {title:?})");
            }
            decision == phodder_core::filter::FilterDecision::Allow
        }
        None => {
            tracing::debug!("episode {episode_label}: not downloading, its podcast's filter rules couldn't be compiled");
            false
        }
    }
}

/// In-progress downloads and their abort handles, reachable from delete paths
/// that have no other link to the downloader.
///
/// Deleting an episode or subscription mid-download let the task finish
/// against a deleted row. `episodes.id` has no AUTOINCREMENT, so SQLite
/// reuses ids, and the stale task's `UPDATE ... WHERE id` and enqueue hit a
/// newer, unrelated episode, which appeared in the queue.
///
/// std::sync::Mutex: held only for map operations, so `cancel_download` stays
/// synchronous and callable from synchronous delete code.
static ACTIVE_DOWNLOADS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<i64, tokio::task::AbortHandle>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub(crate) fn cancel_all_downloads() {
    let ids: Vec<i64> = ACTIVE_DOWNLOADS.lock().expect("ACTIVE_DOWNLOADS mutex shouldn't be poisoned").keys().copied().collect();
    for id in ids {
        cancel_download(id);
    }
}

/// No-op when nothing is downloading. Must run before the database delete;
/// see ACTIVE_DOWNLOADS.
pub(crate) fn cancel_download(episode_id: i64) {
    if let Some(handle) = ACTIVE_DOWNLOADS.lock().expect("ACTIVE_DOWNLOADS mutex shouldn't be poisoned").remove(&episode_id) {
        handle.abort();
        tracing::info!("cancelled in-progress download for episode {episode_id}");
        // Removes a partial file. The extension depends on the response's
        // content type, so the directory is scanned for "episode-{id}.*".
        let prefix = format!("episode-{episode_id}.");
        if let Ok(entries) = std::fs::read_dir(download_dir()) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    if let Err(e) = std::fs::remove_file(entry.path()) {
                        tracing::warn!("failed to remove partial download file {:?} for cancelled episode {episode_id}: {e}", entry.path());
                    }
                }
            }
        }
    }
}

/// `<data dir>/downloads`. Callers create it.
pub(crate) fn download_dir() -> std::path::PathBuf {
    crate::data_dir().join("downloads")
}

/// Feed XML cache, kept out of the downloads directory: it's disposable and
/// shouldn't mix with episodes.
fn feed_cache_dir() -> std::path::PathBuf {
    crate::data_dir().join("feed_cache")
}

/// A hash, since URLs can be long and contain characters unsafe in file
/// names. A collision only reuses the wrong cached item.
fn url_cache_key(url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Images are downscaled to fit this square before caching: nothing displays
/// them larger, and some feeds ship 1920x1080 artwork.
const MAX_CACHED_IMAGE_DIMENSION: i32 = 256;

/// Fetches an image into the shared cache, keyed by its URL, checking disk
/// first. Every artwork path goes through this, so a URL is downloaded
/// once; a show's new cover art has a new URL and is fetched.
async fn get_or_download_image(client: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    let path = download_dir().join(format!("artwork-{}.jpg", url_cache_key(url)));
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(path.to_string_lossy().to_string());
    }
    // A podcast added from Add Podcast already has its artwork there.
    let discovered = discover_artwork_dir().join(format!("artwork-{}.jpg", url_cache_key(url)));
    if tokio::fs::try_exists(&discovered).await.unwrap_or(false) {
        tokio::fs::create_dir_all(download_dir()).await?;
        if tokio::fs::copy(&discovered, &path).await.is_ok() {
            return Ok(path.to_string_lossy().to_string());
        }
    }
    download_image_to(client, url, &path).await?;
    Ok(path.to_string_lossy().to_string())
}

/// Artwork for directory results, kept apart from subscription artwork so
/// it can be pruned. Files untouched for DISCOVER_ARTWORK_MAX_AGE are
/// deleted at startup; a cache hit refreshes the file's modified time.
fn discover_artwork_dir() -> std::path::PathBuf {
    dirs::cache_dir().map(|dir| dir.join("phodder")).unwrap_or_else(|| crate::data_dir().join("cache")).join("discover")
}

const DISCOVER_ARTWORK_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

pub(crate) async fn fetch_discover_artwork(artwork_url: String) -> Option<String> {
    let path = discover_artwork_dir().join(format!("artwork-{}.jpg", url_cache_key(&artwork_url)));
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        let touched = path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            std::fs::File::options().write(true).open(&touched).and_then(|f| f.set_modified(std::time::SystemTime::now()))
        })
        .await;
        return Some(path.to_string_lossy().to_string());
    }
    match download_image_to(&http_client(), &artwork_url, &path).await {
        Ok(()) => Some(path.to_string_lossy().to_string()),
        Err(e) => {
            tracing::warn!("failed to download directory artwork {artwork_url}: {e}");
            None
        }
    }
}

/// Copies directory-cache artwork into the subscription artwork folder,
/// which isn't pruned. Other paths pass through. None on failure; the
/// first refresh downloads the artwork then.
pub(crate) fn keep_discover_artwork(path: &str) -> Option<String> {
    let source = std::path::Path::new(path);
    if !source.starts_with(discover_artwork_dir()) {
        return Some(path.to_string());
    }
    let destination = download_dir().join(source.file_name()?);
    std::fs::create_dir_all(download_dir()).ok()?;
    std::fs::copy(source, &destination).ok()?;
    Some(destination.to_string_lossy().to_string())
}

pub(crate) async fn prune_discover_artwork() {
    let dir = discover_artwork_dir();
    let removed = tokio::task::spawn_blocking(move || {
        let Ok(entries) = std::fs::read_dir(&dir) else { return 0 };
        let mut removed = 0;
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > DISCOVER_ARTWORK_MAX_AGE);
            if stale && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    })
    .await
    .unwrap_or(0);
    if removed > 0 {
        tracing::info!("pruned {removed} unused directory artwork files");
    }
}

async fn download_image_to(client: &reqwest::Client, url: &str, path: &std::path::Path) -> anyhow::Result<()> {
    let response = client.get(url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("HTTP {} downloading image from {url}", response.status());
    }
    let bytes = response.bytes().await?.to_vec();

    // spawn_blocking: decoding and re-encoding is CPU work.
    let resized_bytes = tokio::task::spawn_blocking(move || resize_image_to_fit(&bytes, MAX_CACHED_IMAGE_DIMENSION))
        .await
        .map_err(|e| anyhow::anyhow!("image resize task panicked: {e}"))??;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, &resized_bytes).await?;
    Ok(())
}

/// Decodes any format gdk-pixbuf recognizes and re-encodes as JPEG, scaled
/// down to fit `max_dimension` with its aspect ratio kept. Never scales up.
fn resize_image_to_fit(bytes: &[u8], max_dimension: i32) -> anyhow::Result<Vec<u8>> {
    let pixbuf = gtk4::gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(bytes.to_vec()))
        .map_err(|e| anyhow::anyhow!("failed to decode image: {e}"))?;

    let (width, height) = (pixbuf.width(), pixbuf.height());
    let scale = (max_dimension as f64 / width.max(height) as f64).min(1.0);

    let final_pixbuf = if scale < 1.0 {
        let target_width = ((width as f64 * scale).round() as i32).max(1);
        let target_height = ((height as f64 * scale).round() as i32).max(1);
        pixbuf
            .scale_simple(target_width, target_height, gtk4::gdk_pixbuf::InterpType::Bilinear)
            .ok_or_else(|| anyhow::anyhow!("failed to scale image to {target_width}x{target_height}"))?
    } else {
        pixbuf
    };

    final_pixbuf
        .save_to_bufferv("jpeg", &[("quality", "85")])
        .map_err(|e| anyhow::anyhow!("failed to encode resized image as JPEG: {e}"))
}

/// Used when `settings.refresh_interval_secs` is unset. The first pass runs
/// immediately at startup. Both respect metered gating; Refresh All bypasses
/// it.
const DEFAULT_REFRESH_INTERVAL_SECS: i64 = 60 * 60;

/// Used when `settings.initial_backfill_limit` is unset: how many existing
/// episodes a new subscription queues. Queuing a long-running show's whole
/// history at once was a reported problem.
const DEFAULT_INITIAL_BACKFILL_LIMIT: i64 = 3;

pub async fn run(
    db: Arc<Mutex<rusqlite::Connection>>,
    download_tx: tokio::sync::broadcast::Sender<i64>,
    ui_tx: async_channel::Sender<crate::ui::UiUpdate>,
) -> anyhow::Result<()> {
    tracing::info!("downloader task starting");

    {
        let conn = db.lock().await;
        match conn.execute("UPDATE episodes SET download_status = 'queued' WHERE download_status = 'downloading'", []) {
            Ok(rows) if rows > 0 => {
                tracing::info!("reset {rows} episode(s) stuck at 'downloading' (from an interrupted previous run) back to 'queued'");
            }
            Ok(_) => {}
            Err(e) => tracing::error!("failed to reset stale 'downloading' episodes on startup: {e}"),
        }
    }
    let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;

    loop {
        run_automatic_pass(&db, &download_tx, &ui_tx).await;

        let interval_secs = {
            let conn = db.lock().await;
            phodder_core::settings::get_setting_i64(&conn, "refresh_interval_secs")
                .unwrap_or(DEFAULT_REFRESH_INTERVAL_SECS)
        };
        tracing::debug!("next scheduled refresh+download pass in {interval_secs}s");
        tokio::time::sleep(std::time::Duration::from_secs(interval_secs.max(0) as u64)).await;
    }
}

/// Settings: whether automatic feed updates and episode downloads run on a
/// metered connection. Updates are small, so they default to on.
pub(crate) const REFRESH_ON_METERED_SETTING: &str = "refresh_on_metered";
pub(crate) const DOWNLOAD_ON_METERED_SETTING: &str = "download_on_metered";

/// What automatic work the current connection allows, as (update feeds,
/// download episodes). User-started refreshes and downloads aren't gated.
pub(crate) async fn automatic_work_allowed(db: &Arc<Mutex<rusqlite::Connection>>) -> (bool, bool) {
    if !crate::network::is_metered_connection().await {
        return (true, true);
    }
    allowed_on_metered(&*db.lock().await)
}

fn allowed_on_metered(conn: &rusqlite::Connection) -> (bool, bool) {
    let setting = |key: &str, default: bool| phodder_core::settings::get_setting(conn, key).map(|v| v == "1").unwrap_or(default);
    (setting(REFRESH_ON_METERED_SETTING, true), setting(DOWNLOAD_ON_METERED_SETTING, false))
}

/// The scheduled pass and the one when the network returns, within what a
/// metered connection allows.
pub(crate) async fn run_automatic_pass(
    db: &Arc<Mutex<rusqlite::Connection>>,
    download_tx: &tokio::sync::broadcast::Sender<i64>,
    ui_tx: &async_channel::Sender<crate::ui::UiUpdate>,
) {
    let (refresh, download) = automatic_work_allowed(db).await;
    if !refresh && !download {
        tracing::info!("skipping automatic refresh/download pass: connection is metered");
        return;
    }
    if !download {
        tracing::info!("metered connection: updating feeds, leaving downloads for later");
    }
    refresh_and_download(db, download_tx, ui_tx, false, refresh, download).await;
}


/// Refresh All: updates and downloads whatever the connection.
pub(crate) async fn refresh_and_download_now(
    db: &Arc<Mutex<rusqlite::Connection>>,
    download_tx: &tokio::sync::broadcast::Sender<i64>,
    ui_tx: &async_channel::Sender<crate::ui::UiUpdate>,
    force_bypass_cache: bool,
) {
    refresh_and_download(db, download_tx, ui_tx, force_bypass_cache, true, true).await;
}

async fn refresh_and_download(
    db: &Arc<Mutex<rusqlite::Connection>>,
    download_tx: &tokio::sync::broadcast::Sender<i64>,
    ui_tx: &async_channel::Sender<crate::ui::UiUpdate>,
    force_bypass_cache: bool,
    refresh: bool,
    download: bool,
) {
    crate::feed_health::wait_for_network_state().await;
    if !crate::feed_health::network_available() {
        tracing::info!("skipping refresh/download pass: no network");
        crate::feed_health::send_status(ui_tx).await;
        return;
    }
    if refresh {
        let _activity = RefreshActivity::start(ui_tx);
        if let Err(e) = refresh_all_subscriptions(db, ui_tx, force_bypass_cache).await {
            tracing::error!("feed refresh failed: {e}");
            // Infrastructure failure (listing subscriptions failed). Per-feed
            // failures are tracked in feed_health.
            let _ = ui_tx.send(crate::ui::UiUpdate::Error(crate::i18n::gettext_f("Feed refresh failed: {error}", &[("error", &e.to_string())]))).await;
        }
        // Sync runs between refreshing and downloading, so a remote "played"
        // lands before this pass decides what to download. A no-op without a
        // sync account.
        if let Err(e) = crate::gpodder_sync_runner::sync_all(&crate::db_path(), ui_tx).await {
            tracing::error!("gpodder sync (within the scheduled refresh+download pass) failed: {e}");
        }
    }
    // Episodes found meanwhile stay queued for the next pass that allows it.
    if download {
        if let Err(e) = download_queued_episodes(db, download_tx, ui_tx).await {
            tracing::error!("download pass failed: {e}");
            let _ = ui_tx.send(crate::ui::UiUpdate::Error(crate::i18n::gettext_f("Download pass failed: {error}", &[("error", &e.to_string())]))).await;
        }
    }
    crate::release_free_memory();
}

/// Counts running refreshes, so the Refresh button spins while any runs,
/// including an hourly one overlapping a manual one.
static REFRESHES_RUNNING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(crate) struct RefreshActivity {
    ui_tx: async_channel::Sender<crate::ui::UiUpdate>,
}

impl RefreshActivity {
    pub(crate) fn start(ui_tx: &async_channel::Sender<crate::ui::UiUpdate>) -> Self {
        if REFRESHES_RUNNING.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            let _ = ui_tx.try_send(crate::ui::UiUpdate::Refreshing(true));
        }
        Self { ui_tx: ui_tx.clone() }
    }
}

impl Drop for RefreshActivity {
    fn drop(&mut self) {
        if REFRESHES_RUNNING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            let _ = self.ui_tx.try_send(crate::ui::UiUpdate::Refreshing(false));
        }
    }
}

async fn refresh_all_subscriptions(
    db: &Arc<Mutex<rusqlite::Connection>>,
    ui_tx: &async_channel::Sender<crate::ui::UiUpdate>,
    force_bypass_cache: bool,
) -> anyhow::Result<()> {
    let subscriptions = {
        let conn = db.lock().await;
        phodder_core::subscriptions::list_subscriptions(&conn)?
    };

    tracing::info!("refreshing {} subscription(s)", subscriptions.len());

    let had_any_subscriptions = !subscriptions.is_empty();
    crate::feed_health::retain_only(&subscriptions.iter().map(|s| s.id).collect::<Vec<_>>());

    for sub in subscriptions {
        // One feed's failure doesn't stop the rest. force_bypass_cache is set
        // only by user-started refreshes, whose failures are reported at
        // once.
        match refresh_subscription(db, &sub, ui_tx, force_bypass_cache, false).await {
            Ok(()) => crate::feed_health::record_success(sub.id),
            Err(e) => {
                tracing::error!("failed to refresh subscription {} ({}): {e}", sub.id, sub.feed_url);
                crate::feed_health::record_failure(sub.id, &sub.title, &e, force_bypass_cache);
            }
        }
    }

    if let Err(e) = recache_missing_artwork(db).await {
        tracing::error!("re-caching missing artwork failed: {e}");
    }

    // One DataChanged for the whole pass.
    crate::feed_health::send_status(ui_tx).await;

    if had_any_subscriptions {
        if let Err(e) = ui_tx.send(crate::ui::UiUpdate::DataChanged).await {
            tracing::debug!("no UI listening for DataChanged after refresh-all pass: {e}");
        }
    }

    Ok(())
}

/// Fetches artwork again where the cached file is gone, such as paths from
/// before Phodder ran as a Flatpak. Refreshing a feed re-caches a show's
/// artwork, but an episode's is only cached when the episode is added.
async fn recache_missing_artwork(db: &Arc<Mutex<rusqlite::Connection>>) -> anyhow::Result<()> {
    let rows: Vec<(&'static str, i64, Option<String>, String)> = {
        let conn = db.lock().await;
        let mut rows = Vec::new();
        for table in ["subscriptions", "episodes"] {
            let mut stmt = conn.prepare(&format!("SELECT id, artwork_url, artwork_local_path FROM {table} WHERE artwork_local_path IS NOT NULL"))?;
            let found = stmt.query_map([], |row| Ok((table, row.get(0)?, row.get(1)?, row.get(2)?)))?;
            rows.extend(found.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        rows
    };
    let missing: Vec<_> = rows.into_iter().filter(|(_, _, _, path)| !std::path::Path::new(path).exists()).collect();
    if missing.is_empty() {
        return Ok(());
    }
    tracing::info!("re-caching {} missing artwork file(s)", missing.len());
    let client = http_client();
    for (table, id, url, old_path) in missing {
        // Without a URL there's nothing to fetch; an episode then shows its
        // show's artwork.
        let new_path = match &url {
            Some(url) => match get_or_download_image(&client, url).await {
                Ok(path) => Some(path),
                Err(e) => {
                    tracing::warn!("couldn't re-cache artwork for {table} {id} from {url}: {e}");
                    continue;
                }
            },
            None => None,
        };
        tracing::info!("{table} {id}: artwork {old_path} -> {}", new_path.as_deref().unwrap_or("(none)"));
        db.lock().await.execute(&format!("UPDATE {table} SET artwork_local_path = ?1 WHERE id = ?2"), rusqlite::params![new_path, id])?;
    }
    Ok(())
}

/// Preview metadata plus downloaded artwork, so Manage Subscription opens
/// with the real artwork before the subscription exists.
#[derive(Debug)]
pub(crate) struct FeedPreview {
    pub parsed: phodder_core::feed::ParsedFeed,
    /// `None` when the feed has no artwork or the download failed; a failed
    /// artwork fetch doesn't fail the preview.
    pub artwork_local_path: Option<String>,
}

/// Feed bytes fetched within this window are reused instead of asking the
/// network.
const FEED_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

fn feed_cache_path(url: &str) -> std::path::PathBuf {
    feed_cache_dir().join(format!("feed-{}.xml", url_cache_key(url)))
}

/// Cached bytes for this URL if any caller fetched it within FEED_CACHE_TTL.
/// On disk rather than in memory: mobile targets are memory-constrained, and
/// an unevicted in-memory cache grew without bound. Freshness comes from the
/// file's modified time.
async fn cached_feed_bytes(url: &str) -> Option<Vec<u8>> {
    let path = feed_cache_path(url);
    let metadata = tokio::fs::metadata(&path).await.ok()?;
    let modified = metadata.modified().ok()?;
    // A clock error counts as stale; re-fetching is always safe.
    if modified.elapsed().ok()? >= FEED_CACHE_TTL {
        return None;
    }
    tokio::fs::read(&path).await.ok()
}

/// Separate from refresh_subscription's conditional GET: this cache answers
/// "did anyone fetch this recently" without contacting the server; the
/// conditional GET asks the server whether it changed. Best-effort.
async fn store_feed_bytes(url: &str, bytes: &[u8]) {
    if let Err(e) = tokio::fs::create_dir_all(feed_cache_dir()).await {
        tracing::warn!("failed to create feed cache directory: {e}");
        return;
    }
    if let Err(e) = tokio::fs::write(feed_cache_path(url), bytes).await {
        tracing::warn!("failed to write feed cache for {url}: {e}");
    }
}

/// Fetch and parse shared by fetch_feed_preview and fetch_full_episode_list.
async fn fetch_and_parse_feed(client: &reqwest::Client, url: &str) -> anyhow::Result<phodder_core::feed::ParsedFeed> {
    if let Some(bytes) = cached_feed_bytes(url).await {
        return Ok(phodder_core::feed::parse_feed(&bytes)?);
    }
    let response = client.get(url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("HTTP {} fetching feed", response.status());
    }
    let bytes = response.bytes().await?;
    store_feed_bytes(url, &bytes).await;
    Ok(phodder_core::feed::parse_feed(&bytes)?)
}

/// Fetches a feed for Add Feed without touching the database, so the
/// dialog shows the real title, or a real error, before a subscription
/// exists. Episodes are inserted and queued by the first refresh after
/// Save, once the user has reviewed the settings.
pub(crate) async fn fetch_feed_preview(url: &str) -> anyhow::Result<FeedPreview> {
    fetch_feed_preview_into(url, false).await
}

/// For Add Podcast's previews: the artwork goes to the pruned directory
/// cache, and keep_discover_artwork moves it over if the podcast is added.
pub(crate) async fn fetch_directory_feed_preview(url: &str) -> anyhow::Result<FeedPreview> {
    fetch_feed_preview_into(url, true).await
}

async fn fetch_feed_preview_into(url: &str, directory: bool) -> anyhow::Result<FeedPreview> {
    let client = http_client();
    let parsed = fetch_and_parse_feed(&client, url).await?;

    let artwork_local_path = if let Some(artwork_url) = &parsed.artwork_url {
        let fetched = if directory {
            fetch_discover_artwork(artwork_url.clone()).await.ok_or_else(|| anyhow::anyhow!("download failed"))
        } else {
            get_or_download_image(&client, artwork_url).await
        };
        match fetched {
            Ok(path) => Some(path),
            Err(e) => {
                tracing::warn!("failed to download artwork during feed preview for {url}: {e}");
                None
            }
        }
    } else {
        None
    };

    Ok(FeedPreview { parsed, artwork_local_path })
}

/// The full episode list for browsing a subscription's back catalog. No
/// database writes and no artwork download; episodes are inserted only when
/// the user acts on one (`queue_back_catalog_episode`).
pub(crate) async fn fetch_full_episode_list(feed_url: &str) -> anyhow::Result<phodder_core::feed::ParsedFeed> {
    let client = http_client();
    fetch_and_parse_feed(&client, feed_url).await
}

/// Queues one back-catalog episode and downloads it now instead of waiting
/// for the next scheduled pass. Calls download_queued_episodes directly; a
/// full refresh would re-fetch every feed.
///
/// Inserts the episode if it isn't in the database yet, downloading its
/// artwork as a refresh would; otherwise resets its status to queued.
pub(crate) async fn queue_back_catalog_episode(
    db: &Arc<Mutex<rusqlite::Connection>>,
    subscription_id: i64,
    episode: &phodder_core::feed::ParsedEpisode,
    skip_enqueue: bool,
    download_tx: &tokio::sync::broadcast::Sender<i64>,
    ui_tx: &async_channel::Sender<crate::ui::UiUpdate>,
) -> anyhow::Result<()> {
    let existing_episode_id: Option<i64> = {
        let conn = db.lock().await;
        conn.query_row(
            "SELECT id FROM episodes WHERE subscription_id = ?1 AND guid = ?2",
            rusqlite::params![subscription_id, episode.guid],
            |row| row.get(0),
        )
        .optional()?
    };

    match existing_episode_id {
        Some(episode_id) => {
            let conn = db.lock().await;
            conn.execute(
                "UPDATE episodes SET download_status = 'queued', manually_downloaded = 1, skip_auto_enqueue = ?2 WHERE id = ?1",
                rusqlite::params![episode_id, skip_enqueue],
            )?;
        }
        None => {
            let artwork_local_path = if let Some(artwork_url) = &episode.artwork_url {
                let client = http_client();
                match get_or_download_image(&client, artwork_url).await {
                    Ok(path) => Some(path),
                    Err(e) => {
                        tracing::warn!("failed to download artwork for queued back-catalog episode {}: {e}", episode.guid);
                        None
                    }
                }
            } else {
                None
            };
            let conn = db.lock().await;
            let new_episode_id =
                phodder_core::subscriptions::insert_episode(&conn, subscription_id, episode, true, artwork_local_path.as_deref())?;
            // Manual downloads are exempt from retention, which otherwise
            // evicted them right after the user chose them. Set here rather
            // than in insert_episode, which the automatic backfill also uses.
            // skip_auto_enqueue supports download-only.
            conn.execute(
                "UPDATE episodes SET manually_downloaded = 1, skip_auto_enqueue = ?2 WHERE id = ?1",
                rusqlite::params![new_episode_id, skip_enqueue],
            )?;
        }
    }

    let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
    download_queued_episodes(db, download_tx, ui_tx).await
}

/// One episode's artwork for the back-catalog list, spawned per episode so
/// each thumbnail appears as soon as it's ready; a batched version showed
/// them in clumps. No database writes.
pub(crate) async fn fetch_episode_artwork(guid: String, artwork_url: String) -> Option<String> {
    let client = http_client();
    match get_or_download_image(&client, &artwork_url).await {
        Ok(path) => Some(path),
        Err(e) => {
            tracing::warn!("failed to download episode artwork for {guid}: {e}");
            None
        }
    }
}

/// pub(crate) for manual refreshes (a newly added feed), which run outside
/// the metered-gated scheduled pass.
pub(crate) async fn refresh_subscription(
    db: &Arc<Mutex<rusqlite::Connection>>,
    sub: &phodder_core::models::Subscription,
    ui_tx: &async_channel::Sender<crate::ui::UiUpdate>,
    force_bypass_cache: bool,
    notify_ui: bool,
) -> anyhow::Result<()> {
    let client = http_client();

    let (prior_etag, prior_last_modified) = {
        let conn = db.lock().await;
        conn.query_row(
            "SELECT etag, last_modified FROM subscriptions WHERE id = ?1",
            [sub.id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)),
        )?
    };

    // The shared feed cache is checked first unless force_bypass_cache is
    // set: a user-started refresh means "check now". The conditional GET
    // below still applies either way and can return 304.
    let cached = if force_bypass_cache { None } else { cached_feed_bytes(&sub.feed_url).await };
    let (bytes, etag, last_modified) = if let Some(cached) = cached {
        (cached, prior_etag.clone(), prior_last_modified.clone())
    } else {
        // This subscription's own etag/Last-Modified still apply when the
        // shared cache misses: a 304 skips downloading the body.
        let mut request = client.get(&sub.feed_url);
        if let Some(etag) = &prior_etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = &prior_last_modified {
            request = request.header(reqwest::header::IF_MODIFIED_SINCE, last_modified);
        }

        let response = request.send().await?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            tracing::debug!("subscription {} ({}) unchanged (304)", sub.id, sub.feed_url);
            let conn = db.lock().await;
            phodder_core::subscriptions::record_refresh(
                &conn,
                sub.id,
                prior_etag.as_deref(),
                prior_last_modified.as_deref(),
            )?;
            return Ok(());
        }

        if !response.status().is_success() {
            anyhow::bail!("HTTP {} fetching {}", response.status(), sub.feed_url);
        }

        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let last_modified = response
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let bytes = response.bytes().await?.to_vec();
        store_feed_bytes(&sub.feed_url, &bytes).await;

        (bytes, etag, last_modified)
    };

    // Read once for both the early-stop decision and new-episode filtering.
    let (existing_guids, last_full_refresh_at) = {
        let conn = db.lock().await;
        let guids = phodder_core::subscriptions::existing_episode_guids(&conn, sub.id)?;
        let last_full: Option<String> = conn.query_row(
            "SELECT last_full_refresh_at FROM subscriptions WHERE id = ?1",
            [sub.id],
            |row| row.get(0),
        )?;
        (guids, last_full)
    };
    let is_first_refresh = existing_guids.is_empty();

    // Early stopping can miss a feed that reordered or backfilled an old
    // episode, so a full parse runs at least this often per subscription.
    const FULL_REFRESH_INTERVAL_HOURS: i64 = 24;
    let full_refresh_is_due = match &last_full_refresh_at {
        None => true,
        Some(ts) => match chrono::DateTime::parse_from_rfc3339(ts) {
            Ok(dt) => chrono::Utc::now().signed_duration_since(dt) >= chrono::Duration::hours(FULL_REFRESH_INTERVAL_HOURS),
            Err(_) => true,
        },
    };

    // Not used on the first refresh (the initial backfill needs every
    // episode) or when a full refresh is due.
    let mut used_early_stop = false;
    let parsed: phodder_core::feed::ParsedFeed = if !is_first_refresh && !full_refresh_is_due {
        // Several consecutive known episodes, not one, before stopping: RSS
        // doesn't guarantee newest-first, and this tolerates an occasional
        // out-of-order item. The daily full parse catches the rest.
        const CONSECUTIVE_KNOWN_THRESHOLD: usize = 3;
        let mut consecutive_known = 0usize;
        let outcome = phodder_core::feed::parse_feed_early_stop(&bytes, |episode| {
            if existing_guids.contains(&episode.guid) {
                consecutive_known += 1;
            } else {
                consecutive_known = 0;
            }
            consecutive_known >= CONSECUTIVE_KNOWN_THRESHOLD
        });

        match outcome {
            phodder_core::feed::EarlyStopOutcome::Stopped { episodes, metadata }
            | phodder_core::feed::EarlyStopOutcome::ExhaustedFeed { episodes, metadata } => match metadata.title {
                Some(title) => {
                    used_early_stop = true;
                    phodder_core::feed::ParsedFeed { title, artwork_url: metadata.artwork_url, description: metadata.description, episodes }
                }
                // The early-stop parse found no usable title, which the
                // subscription's metadata update needs.
                None => phodder_core::feed::parse_feed(&bytes)?,
            },
            phodder_core::feed::EarlyStopOutcome::Unsupported => phodder_core::feed::parse_feed(&bytes)?,
        }
    } else {
        phodder_core::feed::parse_feed(&bytes)?
    };

    if !used_early_stop {
        let conn = db.lock().await;
        conn.execute(
            "UPDATE subscriptions SET last_full_refresh_at = ?1 WHERE id = ?2",
            rusqlite::params![chrono::Utc::now().to_rfc3339(), sub.id],
        )?;
    }

    // get_or_download_image skips cached URLs and refetches when the URL
    // changes. Called without the database lock held: it's a network call.
    let artwork_local_path = match &parsed.artwork_url {
        Some(url) => match get_or_download_image(&client, url).await {
            Ok(path) => Some(path),
            Err(e) => {
                tracing::warn!("failed to download artwork for subscription {}: {e}", sub.id);
                // Keeps the existing thumbnail on a transient failure.
                sub.artwork_local_path.clone()
            }
        },
        None => sub.artwork_local_path.clone(),
    };

    // Title and artwork follow the feed on every refresh; a newly added feed
    // starts with its URL as the title.
    {
        let conn = db.lock().await;
        phodder_core::subscriptions::update_subscription_metadata(
            &conn,
            sub.id,
            &parsed.title,
            parsed.artwork_url.as_deref(),
            parsed.description.as_deref(),
            artwork_local_path.as_deref(),
        )?;
    }

    let compiled_filter_rules = if sub.auto_download_enabled {
        // Compiled once per refresh, not per episode.
        Some(phodder_core::filter::compile_rules(&sub.download_filter_rules)?)
    } else {
        None
    };

    // Phase 1: which episodes are new and whether each is auto-queued. In
    // memory, no lock held.
    let new_episodes: Vec<(&phodder_core::feed::ParsedEpisode, bool)> = {
        let conn = db.lock().await;

        // A subscription with no episodes is on its first refresh, where
        // every feed episode is new; that queued whole back catalogs when a
        // show was added.
        let backfill_limit: usize = if is_first_refresh {
            phodder_core::settings::get_setting_i64(&conn, "initial_backfill_limit")
                .unwrap_or(DEFAULT_INITIAL_BACKFILL_LIMIT)
                .max(0) as usize
        } else {
            usize::MAX
        };

        // Filtered first, then the N most recent by publish date (feed order
        // isn't guaranteed). Taking the N most recent first wasted backfill
        // slots on episodes the filters block.
        //
        // Episodes already past the subscription's delete_after_minutes
        // window are left out: they'd be evicted right after downloading.
        // With a short expiry a new subscription may queue nothing, which is
        // correct.
        let mut filter_passing_new_episodes: Vec<&phodder_core::feed::ParsedEpisode> = Vec::new();
        for episode in &parsed.episodes {
            if existing_guids.contains(&episode.guid) {
                continue;
            }
            let show_notes_text = episode.show_notes_html.as_deref().unwrap_or("");
            if episode_qualifies_for_auto_download(
                &episode.guid,
                sub.auto_download_enabled,
                sub.delete_after_minutes,
                compiled_filter_rules.as_deref(),
                episode.publish_date,
                &episode.title,
                show_notes_text,
                false, // A newly discovered episode has no local row, so it isn't played.
            ) {
                filter_passing_new_episodes.push(episode);
            }
        }
        filter_passing_new_episodes.sort_by(|a, b| b.publish_date.cmp(&a.publish_date));
        // The backfill limit applies on the first refresh only (usize::MAX
        // afterwards).
        let queued_guids: std::collections::HashSet<&str> =
            filter_passing_new_episodes.iter().take(backfill_limit).map(|e| e.guid.as_str()).collect();

        parsed
            .episodes
            .iter()
            .filter(|e| !existing_guids.contains(&e.guid))
            .map(|episode| {
                let queued_for_download =
                    compiled_filter_rules.is_some() && queued_guids.contains(episode.guid.as_str());
                (episode, queued_for_download)
            })
            .collect()
    };

    // Per-episode artwork isn't fetched during refresh: for a new
    // subscription that meant hundreds of sequential downloads before
    // anything showed. Lists fall back to the show's artwork, and
    // back-catalog browsing fetches per episode on demand.
    let new_episodes_with_artwork: Vec<(&phodder_core::feed::ParsedEpisode, bool, Option<String>)> =
        new_episodes.into_iter().map(|(episode, queued_for_download)| (episode, queued_for_download, None)).collect();

    // Phase 3: insert, with the lock held only for this synchronous phase.
    let new_count = new_episodes_with_artwork.len();
    {
        let conn = db.lock().await;
        for (episode, queued_for_download, episode_artwork_local_path) in &new_episodes_with_artwork {
            phodder_core::subscriptions::insert_episode(
                &conn,
                sub.id,
                episode,
                *queued_for_download,
                episode_artwork_local_path.as_deref(),
            )?;
        }
        phodder_core::subscriptions::record_refresh(&conn, sub.id, etag.as_deref(), last_modified.as_deref())?;
    }

    // A podcast's first refresh brings in its back catalog, which isn't news.
    if sub.notify_on_new_episodes && !is_first_refresh && new_count > 0 {
        let mut newest_first: Vec<&phodder_core::feed::ParsedEpisode> =
            new_episodes_with_artwork.iter().map(|(episode, _, _)| *episode).collect();
        newest_first.sort_by(|a, b| b.publish_date.cmp(&a.publish_date));
        let update = crate::ui::UiUpdate::NewEpisodes {
            subscription_id: sub.id,
            podcast_title: parsed.title.clone(),
            episode_titles: newest_first.iter().map(|episode| episode.title.clone()).collect(),
        };
        if let Err(e) = ui_tx.send(update).await {
            tracing::debug!("no UI listening for new-episode notification: {e}");
        }
    }

    tracing::info!(
        "refreshed subscription {} ({}): {new_count} new episode(s)",
        sub.id,
        sub.feed_url
    );

    // Sent even without new episodes: title and artwork changes count.
    // refresh_all_subscriptions passes notify_ui = false and sends one
    // DataChanged after the whole pass; one per subscription rebuilt every
    // tab about once a second during a refresh.
    if notify_ui {
        if let Err(e) = ui_tx.send(crate::ui::UiUpdate::DataChanged).await {
            tracing::debug!("no UI listening for DataChanged after refresh: {e}");
        }
    }

    Ok(())
}

/// Marks insufficient disk space so the retry loop can downcast for it and
/// skip backoff; a timer can't fix a full disk. Raised by the preflight check
/// and by ENOSPC mid-download.
#[derive(Debug)]
struct InsufficientStorage;
impl std::fmt::Display for InsufficientStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "insufficient disk space")
    }
}
impl std::error::Error for InsufficientStorage {}

/// Used when `settings.download_max_retries` is unset.
const DEFAULT_MAX_RETRIES: i64 = 5;

/// Backoff: 30s, 1m, 5m, 15m, then 30m.
fn backoff_duration(attempt: i64) -> std::time::Duration {
    let secs = match attempt {
        1 => 30,
        2 => 60,
        3 => 300,
        4 => 900,
        _ => 1800,
    };
    std::time::Duration::from_secs(secs)
}

/// Downloads one manually requested episode right away, skipping the batch's
/// qualification re-check: a manual download overrides the filters.
pub(crate) async fn download_single_episode_directly(
    db: &Arc<Mutex<rusqlite::Connection>>,
    download_tx: &tokio::sync::broadcast::Sender<i64>,
    ui_tx: &async_channel::Sender<crate::ui::UiUpdate>,
    episode_id: i64,
) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(download_dir()).await?;
    let semaphore = Arc::new(Semaphore::new(1));
    download_episode_with_retry(db.clone(), semaphore, download_tx.clone(), ui_tx.clone(), episode_id).await;
    Ok(())
}

/// Downloads every `queued` episode, up to the network-dependent
/// concurrency limit (network.rs). Each episode retries on its own
/// schedule (`download_episode_with_retry`). pub(crate) so a back-catalog
/// queue can start downloading without refreshing every feed first.
pub(crate) async fn download_queued_episodes(
    db: &Arc<Mutex<rusqlite::Connection>>,
    download_tx: &tokio::sync::broadcast::Sender<i64>,
    ui_tx: &async_channel::Sender<crate::ui::UiUpdate>,
) -> anyhow::Result<()> {
    let queued = {
        let conn = db.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, audio_url, artwork_url, subscription_id, publish_date, title, show_notes_text,
                    manually_downloaded OR group_queue_position IS NOT NULL, played
             FROM episodes WHERE download_status = 'queued'",
        )?;
        let rows: Vec<(i64, String, Option<String>, i64, String, String, Option<String>, bool, bool)> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };

    if queued.is_empty() {
        tracing::debug!("no queued episodes to download");
        return Ok(());
    }

    let mut to_download = Vec::with_capacity(queued.len());
    let mut any_disqualified = false;
    {
        let conn = db.lock().await;
        // An episode already in the play queue (restored from a backup
        // without its file) was wanted, like a manual download.
        for (episode_id, audio_url, artwork_url, subscription_id, publish_date_str, title, show_notes_text, wanted, played) in queued {
            if wanted || crate::redownload::is_pending(episode_id) {
                tracing::debug!("episode {episode_id}: downloading without re-checking its podcast's rules (manual, queued for play, or re-download)");
                to_download.push((episode_id, audio_url, artwork_url));
                continue;
            }

            let still_qualifies = match phodder_core::subscriptions::get_subscription(&conn, subscription_id) {
                Ok(Some(sub)) => {
                    match chrono::DateTime::parse_from_rfc3339(&publish_date_str) {
                        Ok(publish_date) => {
                            let compiled_filter_rules = if sub.auto_download_enabled {
                                match phodder_core::filter::compile_rules(&sub.download_filter_rules) {
                                    Ok(compiled) => Some(compiled),
                                    Err(e) => {
                                        tracing::warn!(
                                            "episode {episode_id}: couldn't compile podcast {subscription_id}'s filter rules: {e}"
                                        );
                                        None
                                    }
                                }
                            } else {
                                None
                            };
                            episode_qualifies_for_auto_download(
                                &episode_id.to_string(),
                                sub.auto_download_enabled,
                                sub.delete_after_minutes,
                                compiled_filter_rules.as_deref(),
                                publish_date.with_timezone(&chrono::Utc),
                                &title,
                                show_notes_text.as_deref().unwrap_or(""),
                                played,
                            )
                        }
                        Err(e) => {
                            tracing::warn!(
                                "episode {episode_id}: downloading anyway, couldn't read its publish date {publish_date_str:?}: {e}"
                            );
                            true
                        }
                    }
                }
                Ok(None) => {
                    tracing::warn!("episode {episode_id}: not downloading, its podcast {subscription_id} no longer exists");
                    false
                }
                Err(e) => {
                    tracing::error!("episode {episode_id}: downloading anyway, couldn't look up its podcast {subscription_id}: {e}");
                    true
                }
            };

            if still_qualifies {
                to_download.push((episode_id, audio_url, artwork_url));
            } else {
                any_disqualified = true;
                tracing::info!("episode {episode_id}: no longer qualifies for automatic download; removed from the download queue");
                if let Err(e) = conn.execute("UPDATE episodes SET download_status = 'none' WHERE id = ?1", rusqlite::params![episode_id]) {
                    tracing::error!("failed to dequeue no-longer-qualifying episode {episode_id}: {e}");
                }
            }
        }
    }
    if any_disqualified {
        let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
    }
    if to_download.is_empty() {
        tracing::debug!("no queued episodes still qualify for download after re-evaluation");
        return Ok(());
    }
    tracing::info!("downloading {} queued episode(s)", to_download.len());

    tokio::fs::create_dir_all(download_dir()).await?;

    // Checked once per batch.
    let max_concurrent = crate::network::detect_max_concurrent_downloads().await;
    let semaphore = Arc::new(Semaphore::new(max_concurrent));
    let mut handles = Vec::new();

    for (episode_id, _audio_url, _artwork_url) in to_download {
        let db = db.clone();
        let semaphore = semaphore.clone();
        let download_tx = download_tx.clone();
        let ui_tx = ui_tx.clone();
        let join_handle = tokio::spawn(async move {
            download_episode_with_retry(db, semaphore, download_tx, ui_tx, episode_id).await;
            // The task removes its own entry; racing cancel_download is
            // harmless, since whichever removes it first wins.
            ACTIVE_DOWNLOADS.lock().expect("ACTIVE_DOWNLOADS mutex shouldn't be poisoned").remove(&episode_id);
        });
        // An AbortHandle needs the spawned JoinHandle, so this follows the
        // spawn. A new Tokio task doesn't preempt the spawning thread, so
        // this insert runs before the task's own removal.
        ACTIVE_DOWNLOADS.lock().expect("ACTIVE_DOWNLOADS mutex shouldn't be poisoned").insert(episode_id, join_handle.abort_handle());
        handles.push((episode_id, join_handle));
    }

    for (episode_id, handle) in handles {
        // A panic and a cancellation (cancel_download on delete) both arrive
        // as Err. Only a panic is an error worth a banner;
        // JoinError::is_cancelled() tells them apart.
        if let Err(e) = handle.await {
            if e.is_cancelled() {
                tracing::info!("a download task was cancelled (expected -- its own episode or subscription was deleted)");
            } else {
                tracing::error!("episode {episode_id}'s download task panicked: {e}");
                let conn = db.lock().await;
                let _ = conn.execute(
                    "UPDATE episodes SET download_status = 'failed', last_error_reason = 'download task crashed unexpectedly' WHERE id = ?1",
                    rusqlite::params![episode_id],
                );
                drop(conn);
                let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
                let _ = ui_tx.send(crate::ui::UiUpdate::Error(crate::i18n::gettext("A download crashed unexpectedly"))).await;
            }
        }
    }

    Ok(())
}

/// Retries one episode with exponential backoff, up to
/// `settings.download_max_retries` attempts. Insufficient storage is terminal
/// at once.
///
/// The semaphore permit is taken per attempt, not held through the backoff
/// sleep, so waiting episodes don't block a download slot.
async fn download_episode_with_retry(
    db: Arc<Mutex<rusqlite::Connection>>,
    semaphore: Arc<Semaphore>,
    download_tx: tokio::sync::broadcast::Sender<i64>,
    ui_tx: async_channel::Sender<crate::ui::UiUpdate>,
    episode_id: i64,
) {
    let max_retries = {
        let conn = db.lock().await;
        phodder_core::settings::get_setting_i64(&conn, "download_max_retries")
            .unwrap_or(DEFAULT_MAX_RETRIES)
    };

    let mut attempt: i64 = 0;
    loop {
        let (audio_url, artwork_url): (String, Option<String>) = {
            let conn = db.lock().await;
            match conn.query_row(
                "SELECT audio_url, artwork_url FROM episodes WHERE id = ?1",
                rusqlite::params![episode_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ) {
                Ok(row) => row,
                Err(e) => {
                    tracing::error!("episode {episode_id} disappeared before attempt {attempt} could even start: {e}");
                    return;
                }
            }
        };
        let result = {
            let _permit = semaphore.acquire().await.expect("semaphore is never closed");
            download_episode(&db, &ui_tx, episode_id, &audio_url, artwork_url.as_deref()).await
        };

        match result {
            Ok(()) => {
                let _ = download_tx.send(episode_id);
                if let Err(e) = ui_tx.send(crate::ui::UiUpdate::DataChanged).await {
                    tracing::debug!("no UI listening for DataChanged after download: {e}");
                }
                return;
            }
            Err(e) => {
                let is_insufficient_storage = e.downcast_ref::<InsufficientStorage>().is_some();
                let is_gone_from_feed = e.downcast_ref::<EpisodeGoneFromFeed>().is_some();
                let conn = db.lock().await;

                if is_insufficient_storage {
                    tracing::error!("episode {episode_id} failed permanently (storage): {e}");
                    let _ = conn.execute(
                        "UPDATE episodes SET download_status = 'failed', last_error_reason = ?1 WHERE id = ?2",
                        rusqlite::params![e.to_string(), episode_id],
                    );
                    let display_name = episode_display_name(&conn, episode_id);
                    drop(conn);
                    // DataChanged so the Library row leaves "Downloading
                    // (0%)"; without it the row stayed stuck there.
                    let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
                    let _ = ui_tx
                        .send(crate::ui::UiUpdate::Error(crate::i18n::gettext_f("“{episode}” couldn't download: not enough disk space", &[("episode", &display_name)])))
                        .await;
                    return;
                }

                if is_gone_from_feed {
                    if let Err(reset_err) = phodder_core::retention::mark_episode_deleted(&conn, episode_id) {
                        tracing::error!("episode {episode_id}: failed to reset after confirming it's gone from its feed: {reset_err}");
                    }
                    drop(conn);
                    let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
                    return;
                }

                attempt += 1;
                let _ = conn.execute(
                    "UPDATE episodes SET retry_count = ?1, last_error_reason = ?2 WHERE id = ?3",
                    rusqlite::params![attempt, e.to_string(), episode_id],
                );
                drop(conn); // Released before the (possibly long) sleep.

                if attempt >= max_retries {
                    tracing::error!("episode {episode_id} exhausted {max_retries} retries: {e}");
                    let conn = db.lock().await;
                    let _ = conn.execute(
                        "UPDATE episodes SET download_status = 'failed' WHERE id = ?1",
                        rusqlite::params![episode_id],
                    );
                    let display_name = episode_display_name(&conn, episode_id);
                    drop(conn);
                    // As above: the row otherwise stayed at "Downloading
                    // (0%)" after retries ran out.
                    let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;
                    let _ = ui_tx
                        .send(crate::ui::UiUpdate::Error(crate::i18n::gettext_f(
                            "“{episode}” failed to download after {attempts} attempts",
                            &[("episode", &display_name), ("attempts", &max_retries.to_string())],
                        )))
                        .await;
                    return;
                }

                let delay = backoff_duration(attempt);
                tracing::warn!(
                    "episode {episode_id} failed (attempt {attempt}/{max_retries}), retrying in {delay:?}: {e}"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// The episode title for error messages, or "episode {id}" if it can't be
/// read.
fn episode_display_name(conn: &rusqlite::Connection, episode_id: i64) -> String {
    conn.query_row("SELECT title FROM episodes WHERE id = ?1", [episode_id], |row| row.get::<_, String>(0))
        .unwrap_or_else(|_| format!("episode {episode_id}"))
}

/// Measures a downloaded file's duration in milliseconds. spawn_blocking:
/// discovery runs a small pipeline synchronously. The 5-second timeout bounds
/// a corrupt file that hangs discovery.
async fn probe_audio_duration_ms(path: &std::path::Path) -> anyhow::Result<i64> {
    let uri = format!("file://{}", path.to_string_lossy());
    tokio::task::spawn_blocking(move || -> anyhow::Result<i64> {
        let discoverer = gstreamer_pbutils::Discoverer::new(gstreamer::ClockTime::from_seconds(5))
            .map_err(|e| anyhow::anyhow!("failed to create GstDiscoverer: {e}"))?;
        let info = discoverer
            .discover_uri(&uri)
            .map_err(|e| anyhow::anyhow!("failed to discover {uri}: {e}"))?;
        let duration = info
            .duration()
            .ok_or_else(|| anyhow::anyhow!("GstDiscoverer returned no duration for {uri}"))?;
        Ok((duration.mseconds()) as i64)
    })
    .await
    .map_err(|e| anyhow::anyhow!("duration-probe task panicked: {e}"))?
}

/// Fetches and stores Podcasting 2.0 chapters. Best effort: the episode plays
/// without them, and the engine falls back to ID3 chapters.
async fn fetch_chapters(db: Arc<Mutex<rusqlite::Connection>>, episode_id: i64) {
    let url = match phodder_core::chapters::missing_chapters_url(&*db.lock().await, episode_id) {
        Ok(Some(url)) => url,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("failed to look up chapters URL for episode {episode_id}: {e}");
            return;
        }
    };
    let result: anyhow::Result<Vec<phodder_core::chapters::Chapter>> = async {
        let text = http_client().get(&url).send().await?.error_for_status()?.text().await?;
        Ok(phodder_core::chapters::parse_json(&text)?)
    }
    .await;
    match result {
        Ok(chapters) => {
            if let Err(e) = phodder_core::chapters::replace(&*db.lock().await, episode_id, &chapters) {
                tracing::warn!("failed to store chapters for episode {episode_id}: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to fetch chapters for episode {episode_id} from {url}: {e:#}"),
    }
}

async fn download_episode(
    db: &Arc<Mutex<rusqlite::Connection>>,
    ui_tx: &async_channel::Sender<crate::ui::UiUpdate>,
    episode_id: i64,
    audio_url: &str,
    artwork_url: Option<&str>,
) -> anyhow::Result<()> {
    // Marked 'downloading' at the start of every attempt, with a DataChanged
    // so the Library row registers its progress bar. Without this the status
    // went straight from queued to downloaded and progress never showed.
    {
        let conn = db.lock().await;
        conn.execute("UPDATE episodes SET download_status = 'downloading' WHERE id = ?1", rusqlite::params![episode_id])?;
    }
    let _ = ui_tx.send(crate::ui::UiUpdate::DataChanged).await;

    let client = http_client_for_download();
    let mut already_attempted_refresh = false;

    if let Ok(head) = client.head(audio_url).send().await {
        if head.status() == reqwest::StatusCode::FORBIDDEN && !already_attempted_refresh {
            already_attempted_refresh = true;
            if let RefreshOutcome::EpisodeNotFoundInFeed = try_refresh_stale_audio_url(db, episode_id, audio_url).await {
                return Err(anyhow::Error::new(EpisodeGoneFromFeed));
            }
        }
        if let Some(len) = head.headers().get(reqwest::header::CONTENT_LENGTH) {
            if let Ok(expected_bytes) = len.to_str().unwrap_or("").parse::<u64>() {
                let available = available_disk_space(&download_dir())?;
                if expected_bytes > available {
                    return Err(anyhow::Error::new(InsufficientStorage).context(format!(
                        "episode needs ~{expected_bytes} bytes, {available} available"
                    )));
                }
            }
        }
    }

    let response = client.get(audio_url).send().await?;
    if !response.status().is_success() {
        if response.status() == reqwest::StatusCode::FORBIDDEN && !already_attempted_refresh {
            if let RefreshOutcome::EpisodeNotFoundInFeed = try_refresh_stale_audio_url(db, episode_id, audio_url).await {
                return Err(anyhow::Error::new(EpisodeGoneFromFeed));
            }
        }
        anyhow::bail!("HTTP {} downloading {audio_url}", response.status());
    }

    // Content-Length of the GET being streamed, not the HEAD preflight's,
    // since progress is computed against it. Without one, no progress updates
    // are sent.
    let content_length = response.content_length();

    let extension = audio_file_extension(audio_url, response.headers().get(reqwest::header::CONTENT_TYPE));
    let path = download_dir().join(format!("episode-{episode_id}.{extension}"));
    let mut file = tokio::fs::File::create(&path).await?;

    let mut stream = response.bytes_stream();
    let mut total_bytes: i64 = 0;
    let mut last_reported_percent: u8 = 0;
    const CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
    loop {
        let chunk = match tokio::time::timeout(CHUNK_IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(chunk_result)) => chunk_result,
            Ok(None) => break,
            Err(_) => {
                anyhow::bail!(
                    "no data received for {CHUNK_IDLE_TIMEOUT:?} -- the connection appears to have stalled mid-download"
                );
            }
        };
        let chunk = chunk?;
        if let Err(e) = file.write_all(&chunk).await {
            if e.kind() == std::io::ErrorKind::StorageFull || e.raw_os_error() == Some(28 /* ENOSPC */) {
                // ENOSPC mid-download fails at once with the same marker as
                // the preflight, after removing the partial file.
                drop(file);
                let _ = tokio::fs::remove_file(&path).await;
                return Err(anyhow::Error::new(InsufficientStorage)
                    .context("disk became full mid-download"));
            }
            return Err(e.into());
        }
        total_bytes += chunk.len() as i64;

        // Sent only when the whole percentage changes; per-chunk updates
        // could be hundreds a second.
        if let Some(total) = content_length {
            if total > 0 {
                let percent = ((total_bytes as u64).saturating_mul(100) / total).min(100) as u8;
                if percent != last_reported_percent {
                    last_reported_percent = percent;
                    let _ = ui_tx
                        .send(crate::ui::UiUpdate::DownloadProgress { episode_id, percent })
                        .await;
                }
            }
        }
    }
    file.flush().await?;

    // Episode artwork is fetched only when the episode downloads.
    // Best-effort: lists fall back to the show's artwork.
    let episode_artwork_local_path = match artwork_url {
        Some(url) => {
            let client = http_client();
            match get_or_download_image(&client, url).await {
                Ok(path) => Some(path),
                Err(e) => {
                    tracing::warn!("failed to download artwork for episode {episode_id}: {e}");
                    None
                }
            }
        }
        None => None,
    };

    // Measured from the file so every list shows the real duration, not the
    // feed's. GstDiscoverer reads it without a full playback pipeline. Done
    // before taking the lock: probing reads the file.
    let probed_duration_ms = probe_audio_duration_ms(&path).await;
    if let Err(ref e) = probed_duration_ms {
        tracing::warn!("failed to probe real duration for episode {episode_id}, keeping feed-provided value if any: {e}");
    }

    let conn = db.lock().await;
    conn.execute(
        "UPDATE episodes
         SET download_status = 'downloaded',
             local_file_path = ?1,
             file_size_bytes = ?2,
             downloaded_at = ?3,
             last_error_reason = NULL,
             artwork_local_path = COALESCE(?5, artwork_local_path),
             duration_ms = COALESCE(?6, duration_ms)
         WHERE id = ?4",
        rusqlite::params![
            path.to_string_lossy(),
            total_bytes,
            chrono::Utc::now().to_rfc3339(),
            episode_id,
            episode_artwork_local_path,
            probed_duration_ms.ok(),
        ],
    )?;

    // Every completed download joins its group's queue; queueing only set
    // download_status, which left the Queue tab empty.
    //
    // Download-only (skip_auto_enqueue) is the exception. The flag is reset
    // here whichever branch runs, so a later re-download isn't affected.
    tokio::spawn(fetch_chapters(db.clone(), episode_id));
    // Fetched now so it's there offline.
    tokio::spawn(async move {
        crate::transcripts::load(&crate::db_path(), episode_id).await;
    });

    let skip_auto_enqueue: bool =
        conn.query_row("SELECT skip_auto_enqueue FROM episodes WHERE id = ?1", [episode_id], |row| row.get(0))?;
    if skip_auto_enqueue {
        conn.execute("UPDATE episodes SET skip_auto_enqueue = 0 WHERE id = ?1", [episode_id])?;
    } else {
        phodder_core::queue::enqueue_episode(&conn, episode_id)?;
    }

    tracing::info!("downloaded episode {episode_id}: {total_bytes} bytes -> {}", path.display());

    Ok(())
}

enum RefreshOutcome {
    UrlUpdated,
    UrlUnchanged,
    EpisodeNotFoundInFeed,
    CouldNotCheck,
}

async fn try_refresh_stale_audio_url(db: &Arc<Mutex<rusqlite::Connection>>, episode_id: i64, current_audio_url: &str) -> RefreshOutcome {
    let (_subscription_id, guid, feed_url): (i64, String, String) = {
        let conn = db.lock().await;
        match conn.query_row(
            "SELECT e.subscription_id, e.guid, s.feed_url FROM episodes e JOIN subscriptions s ON s.id = e.subscription_id WHERE e.id = ?1",
            rusqlite::params![episode_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ) {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!("episode_id={episode_id}: couldn't look up subscription/guid to attempt a URL refresh: {e}");
                return RefreshOutcome::CouldNotCheck;
            }
        }
    };
    let parsed = match fetch_full_episode_list(&feed_url).await {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!("episode_id={episode_id}: URL refresh attempt failed -- couldn't fetch/parse feed {feed_url:?}: {e}");
            return RefreshOutcome::CouldNotCheck;
        }
    };
    let Some(fresh_episode) = parsed.episodes.iter().find(|e| e.guid == guid) else {
        tracing::warn!("episode_id={episode_id}: guid={guid:?} NOT FOUND in feed {feed_url:?} -- treating as confirmed gone, no further retries will help");
        return RefreshOutcome::EpisodeNotFoundInFeed;
    };
    if fresh_episode.audio_url == current_audio_url {
        return RefreshOutcome::UrlUnchanged;
    }
    tracing::info!(
        "episode_id={episode_id}: URL refresh found a DIFFERENT audio_url in the feed -- updating stored audio_url. old={current_audio_url:?} new={:?}",
        fresh_episode.audio_url
    );
    let conn = db.lock().await;
    if let Err(e) =
        conn.execute("UPDATE episodes SET audio_url = ?1 WHERE id = ?2", rusqlite::params![fresh_episode.audio_url, episode_id])
    {
        tracing::error!("episode_id={episode_id}: failed to persist refreshed audio_url: {e}");
        return RefreshOutcome::CouldNotCheck;
    }
    RefreshOutcome::UrlUpdated
}

/// Prefers the URL's extension, then Content-Type, then ".audio". GStreamer
/// sniffs content, so this only matters to someone browsing the files.
fn audio_file_extension(audio_url: &str, content_type: Option<&reqwest::header::HeaderValue>) -> &'static str {
    const KNOWN_AUDIO_EXTENSIONS: &[&str] = &["mp3", "m4a", "m4b", "aac", "ogg", "oga", "opus", "wav", "flac"];

    // Enclosure URLs often carry signed or tracking query strings.
    let path_only = audio_url.split(['?', '#']).next().unwrap_or(audio_url);
    if let Some(ext) = std::path::Path::new(path_only).extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_ascii_lowercase();
        for &known in KNOWN_AUDIO_EXTENSIONS {
            if known == ext_lower {
                return known;
            }
        }
    }

    if let Some(content_type) = content_type.and_then(|v| v.to_str().ok()) {
        // Drops parameters such as `; charset=...`.
        let mime = content_type.split(';').next().unwrap_or(content_type).trim();
        return match mime {
            "audio/mpeg" => "mp3",
            "audio/mp4" | "audio/x-m4a" => "m4a",
            "audio/aac" | "audio/aacp" => "aac",
            "audio/ogg" | "application/ogg" => "ogg",
            "audio/opus" => "opus",
            "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
            "audio/flac" | "audio/x-flac" => "flac",
            _ => "audio",
        };
    }

    "audio"
}

/// Bytes available on the filesystem containing `path`, via statvfs.
/// Linux-only, matching the target platform.
fn available_disk_space(path: &std::path::Path) -> anyhow::Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path)?;
    Ok(stat.blocks_available() as u64 * stat.fragment_size() as u64)
}

#[cfg(test)]
mod metered_tests {
    use super::*;

    #[test]
    fn metered_connections_update_feeds_but_hold_downloads_by_default() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        phodder_core::db::migrate(&conn).unwrap();
        assert_eq!(allowed_on_metered(&conn), (true, false));

        phodder_core::settings::set_setting(&conn, DOWNLOAD_ON_METERED_SETTING, "1").unwrap();
        phodder_core::settings::set_setting(&conn, REFRESH_ON_METERED_SETTING, "0").unwrap();
        assert_eq!(allowed_on_metered(&conn), (false, true));
    }
}
