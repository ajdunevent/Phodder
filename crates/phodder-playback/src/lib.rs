//! Playback engine. One task owns the GStreamer pipeline and all playback
//! state; everything else talks to it through [`Player`]. Commands and bus
//! messages are handled in one loop, so they can't interleave.

mod engine;
mod pipeline;
mod store;
mod trim;

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex};

pub use gstreamer;

/// The episode's downloaded file is missing.
#[derive(Debug)]
pub struct FileMissing(pub String);

impl std::fmt::Display for FileMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FileMissing {}

/// Whether a play request failed because the episode's file is missing.
pub fn is_file_missing(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<FileMissing>())
}
pub use phodder_core::chapters::Chapter;

pub const RATE_RANGE: std::ops::RangeInclusive<f64> = 0.5..=4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl Status {
    pub fn as_mpris_str(self) -> &'static str {
        match self {
            Status::Playing => "Playing",
            Status::Paused => "Paused",
            Status::Stopped => "Stopped",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EpisodeInfo {
    pub id: i64,
    pub title: String,
    pub podcast_title: String,
    pub artwork_url: Option<String>,
    pub artwork_local_path: Option<String>,
}

/// Published after every state change and on a short tick while playing.
/// `status` reflects intent: an episode still loading reports Playing if
/// it will start once ready.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub status: Status,
    pub episode: Option<EpisodeInfo>,
    pub position_ms: u64,
    pub position_at: Instant,
    pub duration_ms: Option<u64>,
    pub rate: f64,
    pub sleep_deadline: Option<Instant>,
    /// The sleep timer is set to stop when this episode ends.
    pub sleep_at_episode_end: bool,
    /// Loading or seeking; position is not advancing.
    pub loading: bool,
    /// Stored chapters, or those embedded in the file. Sorted by start.
    pub chapters: Arc<Vec<Chapter>>,
    /// `episode` is a preview streamed before subscribing; it isn't in the
    /// database.
    pub preview: bool,
}

impl Snapshot {
    /// Position extrapolated from the last sample, for consumers polling
    /// between ticks (MPRIS `Position`).
    pub fn position_now_ms(&self) -> u64 {
        let mut pos = self.position_ms;
        if self.status == Status::Playing && !self.loading {
            pos += (self.position_at.elapsed().as_millis() as f64 * self.rate) as u64;
        }
        match self.duration_ms {
            Some(d) => pos.min(d),
            None => pos,
        }
    }

    pub fn current_chapter(&self) -> Option<&Chapter> {
        let pos = self.position_now_ms();
        self.chapters.iter().rev().find(|c| c.start_ms <= pos)
    }

    pub fn sleep_remaining(&self) -> Option<Duration> {
        self.sleep_deadline.map(|d| d.saturating_duration_since(Instant::now()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MilestoneKind {
    Play,
    Pause,
    Seek,
    Stop,
    Finished,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Playback position worth syncing (gpodder "play" action).
    /// `Finished` carries the episode duration as its position.
    Milestone { episode_id: i64, position_ms: u64, kind: MilestoneKind },
    /// A seek completed a jump; MPRIS clients expect a `Seeked` signal.
    Seeked { position_ms: u64 },
    /// Played/queue state changed in the database.
    LibraryChanged,
    /// Loading an episode failed and it was skipped. `file_problem` when
    /// the file is missing or its contents won't decode, which a fresh
    /// download may fix (not for a missing codec or audio output).
    /// `requested` when the user chose it; the previous episode is then
    /// reloaded, paused, instead of playing the next one.
    EpisodeFailed { episode_id: i64, message: String, file_problem: bool, requested: bool },
    PreviewFailed { message: String },
}

#[derive(Debug)]
pub(crate) enum Command {
    Play,
    Pause,
    TogglePlay,
    /// Atomic check-and-pause for watchers that resume only what they paused.
    PauseIfPlaying,
    Stop,
    /// Stops if one of these episodes is loaded; returns whether it stopped.
    StopIfCurrent(Vec<i64>),
    Next,
    /// Restarts the loaded episode (MPRIS Previous).
    Restart,
    PreviousChapter,
    NextChapter,
    SeekTo(u64),
    SeekBy(i64),
    SkipForward,
    SkipBackward,
    SetRate(f64),
    /// Reads a podcast's speed and trimming again (Some), or the player's
    /// Trim Silence setting (None), for the loaded episode.
    ApplyPodcastSettings(Option<i64>),
    /// Replaces an end-of-episode sleep timer too; None cancels both.
    SetSleepTimer(Option<Duration>),
    /// Finishes the current episode, then loads the next one paused.
    SleepAtEpisodeEnd,
    PlayEpisode(i64),
    /// Streams an episode of a podcast that isn't subscribed. Nothing about
    /// it is saved: no position, History or sync.
    PlayPreview { uri: String, info: EpisodeInfo },
    RestoreLastInProgress,
    Shutdown,
}

pub(crate) struct Request {
    pub cmd: Command,
    /// `bool` carries the result of Next (an episode loaded) and
    /// PauseIfPlaying (playback was paused); other commands return true.
    pub reply: Option<oneshot::Sender<anyhow::Result<bool>>>,
}

pub struct Config {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    /// Replaces playbin's automatic audio sink. Tests pass a synced fakesink.
    pub audio_sink: Option<gstreamer::Element>,
}

/// Cheap to clone. Dropping every handle does not stop the engine; send
/// [`Player::shutdown`] to persist position and release the pipeline.
#[derive(Clone)]
pub struct Player {
    tx: mpsc::UnboundedSender<Request>,
    snapshot: watch::Receiver<Snapshot>,
    events: broadcast::Sender<Event>,
    output: pipeline::OutputProbe,
}

impl Player {
    /// Spawns the engine on the current tokio runtime.
    pub fn spawn(config: Config) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(64);
        let output = pipeline::OutputProbe::default();
        let snapshot = engine::spawn(config, rx, events.clone(), output.clone())?;
        Ok(Self { tx, snapshot, events, output })
    }

    /// The sound server device audio is going to (`bluez_output.…` for
    /// Bluetooth), while an episode is loaded. None when nothing is loaded
    /// or the sink can't tell (pipewiresink).
    pub fn output_device(&self) -> Option<String> {
        self.output.current_device()
    }

    /// Resolves when the sink reports another device, or a new sink appears.
    /// Only fires while playing; poll output_device while paused.
    pub async fn output_changed(&self) {
        self.output.changed().await
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<Snapshot> {
        self.snapshot.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn status(&self) -> Status {
        self.snapshot.borrow().status
    }

    pub fn episode_id(&self) -> Option<i64> {
        self.snapshot.borrow().episode.as_ref().map(|e| e.id)
    }

    /// Fire-and-forget; errors are logged by the engine. Ordering between
    /// calls from one caller is preserved.
    fn send(&self, cmd: Command) {
        if self.tx.send(Request { cmd, reply: None }).is_err() {
            tracing::warn!("playback engine is gone; command dropped");
        }
    }

    async fn request(&self, cmd: Command) -> anyhow::Result<bool> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request { cmd, reply: Some(reply) })
            .map_err(|_| anyhow::anyhow!("playback engine is gone"))?;
        rx.await.map_err(|_| anyhow::anyhow!("playback engine dropped the request"))?
    }

    pub async fn play(&self) -> anyhow::Result<()> {
        self.request(Command::Play).await.map(drop)
    }
    pub async fn pause(&self) -> anyhow::Result<()> {
        self.request(Command::Pause).await.map(drop)
    }
    pub async fn toggle_play(&self) -> anyhow::Result<()> {
        self.request(Command::TogglePlay).await.map(drop)
    }
    /// Returns whether playback was playing and is now paused.
    pub async fn pause_if_playing(&self) -> anyhow::Result<bool> {
        self.request(Command::PauseIfPlaying).await
    }
    pub async fn stop(&self) -> anyhow::Result<()> {
        self.request(Command::Stop).await.map(drop)
    }
    /// Stops if one of `episode_ids` is loaded, for episodes the user removed
    /// from the queue or deleted. Returns whether it stopped.
    pub async fn stop_if_current(&self, episode_ids: Vec<i64>) -> anyhow::Result<bool> {
        self.request(Command::StopIfCurrent(episode_ids)).await
    }
    /// Returns false when the queue had nothing else; playback stops.
    pub async fn next(&self) -> anyhow::Result<bool> {
        self.request(Command::Next).await
    }
    pub async fn restart(&self) -> anyhow::Result<()> {
        self.request(Command::Restart).await.map(drop)
    }
    pub async fn previous_chapter(&self) -> anyhow::Result<()> {
        self.request(Command::PreviousChapter).await.map(drop)
    }
    pub async fn next_chapter(&self) -> anyhow::Result<()> {
        self.request(Command::NextChapter).await.map(drop)
    }
    pub async fn seek_to(&self, position_ms: u64) -> anyhow::Result<()> {
        self.request(Command::SeekTo(position_ms)).await.map(drop)
    }
    pub async fn seek_by(&self, offset_ms: i64) -> anyhow::Result<()> {
        self.request(Command::SeekBy(offset_ms)).await.map(drop)
    }
    pub async fn skip_forward(&self) -> anyhow::Result<()> {
        self.request(Command::SkipForward).await.map(drop)
    }
    pub async fn skip_backward(&self) -> anyhow::Result<()> {
        self.request(Command::SkipBackward).await.map(drop)
    }
    pub async fn set_rate(&self, rate: f64) -> anyhow::Result<()> {
        self.request(Command::SetRate(rate)).await.map(drop)
    }
    pub async fn set_sleep_timer(&self, duration: Option<Duration>) -> anyhow::Result<()> {
        self.request(Command::SetSleepTimer(duration)).await.map(drop)
    }
    pub async fn sleep_at_episode_end(&self) -> anyhow::Result<()> {
        self.request(Command::SleepAtEpisodeEnd).await.map(drop)
    }
    pub async fn play_episode(&self, episode_id: i64) -> anyhow::Result<()> {
        self.request(Command::PlayEpisode(episode_id)).await.map(drop)
    }
    pub async fn play_preview(&self, uri: String, info: EpisodeInfo) -> anyhow::Result<()> {
        self.request(Command::PlayPreview { uri, info }).await.map(drop)
    }
    pub async fn restore_last_in_progress(&self) -> anyhow::Result<()> {
        self.request(Command::RestoreLastInProgress).await.map(drop)
    }
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.request(Command::Shutdown).await.map(drop)
    }

    pub fn send_restart(&self) {
        self.send(Command::Restart)
    }
    pub fn send_previous_chapter(&self) {
        self.send(Command::PreviousChapter)
    }
    pub fn send_next_chapter(&self) {
        self.send(Command::NextChapter)
    }
    pub fn send_next(&self) {
        self.send(Command::Next)
    }
    pub fn send_toggle_play(&self) {
        self.send(Command::TogglePlay)
    }
    pub fn send_seek_to(&self, position_ms: u64) {
        self.send(Command::SeekTo(position_ms))
    }
    pub fn send_skip_forward(&self) {
        self.send(Command::SkipForward)
    }
    pub fn send_skip_backward(&self) {
        self.send(Command::SkipBackward)
    }
    pub fn send_set_rate(&self, rate: f64) {
        self.send(Command::SetRate(rate))
    }
    /// After a podcast's playback settings change (Some), or the player's
    /// Trim Silence setting (None).
    pub fn send_apply_podcast_settings(&self, subscription_id: Option<i64>) {
        self.send(Command::ApplyPodcastSettings(subscription_id))
    }
    pub fn send_set_sleep_timer(&self, duration: Option<Duration>) {
        self.send(Command::SetSleepTimer(duration))
    }
    pub fn send_sleep_at_episode_end(&self) {
        self.send(Command::SleepAtEpisodeEnd)
    }
    /// Queued at once, so it keeps its place among other sent commands; the
    /// returned future reports whether the episode loaded.
    pub fn send_play_episode(&self, episode_id: i64) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static {
        let (reply, rx) = oneshot::channel();
        let sent = self.tx.send(Request { cmd: Command::PlayEpisode(episode_id), reply: Some(reply) });
        async move {
            sent.map_err(|_| anyhow::anyhow!("playback engine is gone"))?;
            rx.await.map_err(|_| anyhow::anyhow!("playback engine dropped the request"))?.map(drop)
        }
    }
    pub fn send_stop_if_current(&self, episode_ids: Vec<i64>) {
        self.send(Command::StopIfCurrent(episode_ids))
    }
}
