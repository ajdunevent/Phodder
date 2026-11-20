//! Fetching and caching episode transcripts. The raw file is kept next to
//! the audio as `transcript-<id>.txt`; startup deletes those whose episode
//! has no downloaded file. Parsed transcripts are kept in memory for the
//! few most recent episodes, since a word-level JSON file can be large.

use phodder_core::transcript::Transcript;
use std::sync::{Arc, LazyLock, Mutex};

const MEMORY_CACHE_SIZE: usize = 4;

static PARSED: LazyLock<Mutex<Vec<(i64, Arc<Transcript>)>>> = LazyLock::new(Default::default);

fn cache_path(episode_id: i64) -> std::path::PathBuf {
    crate::downloader::download_dir().join(format!("transcript-{episode_id}.txt"))
}

/// The episode's transcript, from memory, the cached file, or the feed's
/// transcript URL. None when the episode has none or it can't be read.
pub(crate) async fn load(db_path: &std::path::Path, episode_id: i64) -> Option<Arc<Transcript>> {
    if let Some(hit) = PARSED.lock().ok()?.iter().find(|(id, _)| *id == episode_id).map(|(_, t)| t.clone()) {
        return Some(hit);
    }
    let path = cache_path(episode_id);
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(_) => {
            let url = transcript_url(db_path, episode_id)?;
            let text = match fetch(&url).await {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!("failed to fetch transcript for episode {episode_id} from {url}: {e:#}");
                    return None;
                }
            };
            if let Err(e) = tokio::fs::create_dir_all(crate::downloader::download_dir()).await {
                tracing::warn!("couldn't create the downloads directory for a transcript: {e}");
            } else if let Err(e) = tokio::fs::write(&path, &text).await {
                tracing::warn!("couldn't cache the transcript for episode {episode_id}: {e}");
            }
            text
        }
    };
    let transcript = Arc::new(tokio::task::spawn_blocking(move || phodder_core::transcript::parse(&text)).await.ok()??);
    if let Ok(mut parsed) = PARSED.lock() {
        parsed.retain(|(id, _)| *id != episode_id);
        parsed.insert(0, (episode_id, transcript.clone()));
        parsed.truncate(MEMORY_CACHE_SIZE);
    }
    Some(transcript)
}

/// Whether the feed offers a transcript for this episode.
pub(crate) fn has_transcript(conn: &rusqlite::Connection, episode_id: i64) -> bool {
    conn.query_row("SELECT transcript_url IS NOT NULL FROM episodes WHERE id = ?1", [episode_id], |row| row.get(0)).unwrap_or(false)
}

fn transcript_url(db_path: &std::path::Path, episode_id: i64) -> Option<String> {
    let conn = phodder_core::open_connection(db_path).ok()?;
    conn.query_row("SELECT transcript_url FROM episodes WHERE id = ?1", [episode_id], |row| row.get(0)).ok().flatten()
}

async fn fetch(url: &str) -> anyhow::Result<String> {
    Ok(crate::downloader::http_client().get(url).send().await?.error_for_status()?.text().await?)
}

/// Deletes cached transcripts whose episode has no downloaded file.
pub(crate) async fn prune(db_path: std::path::PathBuf) {
    let removed = tokio::task::spawn_blocking(move || {
        let Ok(conn) = phodder_core::open_connection(&db_path) else { return 0 };
        let Ok(entries) = std::fs::read_dir(crate::downloader::download_dir()) else { return 0 };
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(episode_id) = name.strip_prefix("transcript-").and_then(|n| n.strip_suffix(".txt")).and_then(|n| n.parse::<i64>().ok()) else {
                continue;
            };
            let downloaded: bool = conn
                .query_row("SELECT local_file_path IS NOT NULL FROM episodes WHERE id = ?1", [episode_id], |row| row.get(0))
                .unwrap_or(false);
            if !downloaded && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    })
    .await
    .unwrap_or(0);
    if removed > 0 {
        tracing::info!("removed {removed} cached transcript(s) for episodes no longer downloaded");
    }
}
