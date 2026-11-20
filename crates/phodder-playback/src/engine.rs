use crate::pipeline::Pipeline;
use crate::{store, Chapter, Command, Config, EpisodeInfo, Event, MilestoneKind, Request, Snapshot, Status, RATE_RANGE};
use futures_util::{FutureExt, StreamExt};
use gstreamer as gst;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, watch, Mutex};

const TICK: Duration = Duration::from_millis(250);
const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);
const PERSIST_INTERVAL: Duration = Duration::from_secs(20);
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// Fallback for a seek whose ASYNC_DONE never arrives.
const SEEK_TIMEOUT: Duration = Duration::from_secs(3);
/// On resume, a pipeline position this far from where playback was paused
/// is treated as lost. Seen after Bluetooth sinks disconnect and reconnect
/// while paused: playback restarted from 0.
const RESUME_DRIFT_MS: u64 = 3_000;
/// Previous Chapter restarts the current chapter when more than this far
/// into it, and goes to the one before otherwise.
const CHAPTER_RESTART_THRESHOLD_MS: u64 = 3_000;
/// A runtime pipeline error reloads the episode once per window; repeated
/// errors inside it stop playback instead of looping.
const ERROR_RETRY_WINDOW: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq)]
enum LoadStage {
    /// Waiting for the first ASYNC_DONE after opening the file.
    Prerolling,
    /// Waiting for the resume/rate seek to complete.
    Seeking,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Loading { stage: LoadStage, play_when_ready: bool, since: Instant },
    Ready { playing: bool },
}

struct Loaded {
    info: EpisodeInfo,
    chapters: Arc<Vec<Chapter>>,
    played_threshold_ms: Option<u64>,
    threshold_marked: bool,
    /// The chapter playback was in at the last tick; None until the first.
    last_chapter: Option<usize>,
    /// The chapter the user last sought into, which auto-skip leaves alone.
    user_chapter: Option<usize>,
}

struct Engine {
    pipeline: Pipeline,
    bus: gst::bus::BusStream,
    db: Arc<Mutex<rusqlite::Connection>>,
    snapshot_tx: watch::Sender<Snapshot>,
    events: broadcast::Sender<Event>,

    phase: Phase,
    current: Option<Loaded>,
    rate: f64,
    /// Last known position, or the target of a pending load/seek. Only
    /// overwritten by a successful position query, so a failed query never
    /// turns into a saved position of 0.
    position_ms: u64,
    position_at: Instant,
    duration_ms: Option<u64>,
    /// One flushing seek at a time; further requests overwrite
    /// `position_ms` and set `seek_queued`, then go out on ASYNC_DONE.
    seek_in_flight: Option<Instant>,
    seek_queued: bool,
    paused_at_ms: Option<u64>,
    sleep_deadline: Option<Instant>,
    sleep_at_episode_end: bool,
    last_persist: Instant,
    last_publish: Instant,
    last_error_retry: Option<(i64, Instant)>,
    /// Episodes that failed to load this session; skipped when advancing.
    failed: HashSet<i64>,
    /// An episode the user chose, until it has loaded.
    requested: Option<RequestedPlay>,
    /// The media type of the decoder GStreamer reported missing for the
    /// current load. Text or another non-media type means the download
    /// is something else (an HTML error page, say), not an unsupported
    /// codec.
    missing_decoder: Option<String>,
}

/// A file can turn out unplayable only once loading starts, so the queue
/// changes when the chosen episode is ready, and a failure goes back to
/// `previous` instead of playing whatever is next.
#[derive(Clone, Copy)]
struct RequestedPlay {
    episode_id: i64,
    previous: Option<i64>,
}

pub(crate) fn spawn(
    config: Config,
    rx: mpsc::UnboundedReceiver<Request>,
    events: broadcast::Sender<Event>,
    output: crate::pipeline::OutputProbe,
) -> anyhow::Result<watch::Receiver<Snapshot>> {
    let pipeline = Pipeline::new(config.audio_sink, output)?;
    let bus = pipeline.bus().stream();
    let now = Instant::now();
    let (snapshot_tx, snapshot_rx) = watch::channel(Snapshot {
        status: Status::Stopped,
        episode: None,
        position_ms: 0,
        position_at: now,
        duration_ms: None,
        rate: 1.0,
        sleep_deadline: None,
        sleep_at_episode_end: false,
        loading: false,
        chapters: Arc::default(),
    });
    let engine = Engine {
        pipeline,
        bus,
        db: config.db,
        snapshot_tx,
        events,
        phase: Phase::Idle,
        current: None,
        rate: 1.0,
        position_ms: 0,
        position_at: now,
        duration_ms: None,
        seek_in_flight: None,
        seek_queued: false,
        paused_at_ms: None,
        sleep_deadline: None,
        sleep_at_episode_end: false,
        last_persist: now,
        last_publish: now,
        last_error_retry: None,
        failed: HashSet::new(),
        requested: None,
        missing_decoder: None,
    };
    tokio::spawn(engine.run(rx));
    Ok(snapshot_rx)
}

impl Engine {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Request>) {
        self.rate = store::saved_rate(&*self.db.lock().await);
        self.publish();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            let sleep_deadline = self.sleep_deadline;
            tokio::select! {
                request = rx.recv() => {
                    let Some(Request { cmd, reply }) = request else { break };
                    let shutdown = matches!(cmd, Command::Shutdown);
                    tracing::debug!("playback command: {cmd:?}");
                    let result = self.handle(cmd).await;
                    match reply {
                        Some(reply) => {
                            let _ = reply.send(result);
                        }
                        None => {
                            if let Err(e) = result {
                                tracing::error!("playback command failed: {e:#}");
                            }
                        }
                    }
                    if shutdown {
                        break;
                    }
                }
                Some(msg) = self.bus.next() => self.on_bus(msg).await,
                _ = tick.tick() => self.on_tick().await,
                _ = sleep_until(sleep_deadline), if sleep_deadline.is_some() => self.on_sleep_timer().await,
            }
        }

        self.persist().await;
        self.pipeline.unload();
        tracing::info!("playback engine stopped");
    }

    async fn handle(&mut self, cmd: Command) -> anyhow::Result<bool> {
        match cmd {
            Command::Play => self.play().await,
            Command::Pause => self.pause().await,
            Command::TogglePlay => {
                if self.intends_to_play() {
                    self.pause().await
                } else {
                    self.play().await
                }
            }
            Command::PauseIfPlaying => {
                if self.intends_to_play() {
                    self.pause().await
                } else {
                    Ok(false)
                }
            }
            Command::Stop => {
                self.stop().await;
                Ok(true)
            }
            Command::StopIfCurrent(ids) => {
                if self.current_id().is_some_and(|id| ids.contains(&id)) {
                    self.stop().await;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Command::Next => self.next().await,
            Command::Restart => self.user_seek(|e| e.request_seek(0)),
            Command::PreviousChapter => self.user_seek(Self::previous_chapter),
            Command::NextChapter => self.user_seek(Self::next_chapter),
            Command::SeekTo(ms) => self.user_seek(|e| e.request_seek(ms)),
            Command::SeekBy(offset) => self.user_seek(|e| e.seek_by(offset)),
            Command::SkipForward => {
                let ms = store::skip_forward_ms(&*self.db.lock().await);
                self.user_seek(|e| e.seek_by(ms as i64))
            }
            Command::SkipBackward => {
                let ms = store::skip_backward_ms(&*self.db.lock().await);
                self.user_seek(|e| e.seek_by(-(ms as i64)))
            }
            Command::SetRate(rate) => self.set_rate(rate).await,
            Command::SetSleepTimer(duration) => {
                self.sleep_deadline = duration.map(|d| Instant::now() + d);
                self.sleep_at_episode_end = false;
                self.publish();
                Ok(true)
            }
            Command::SleepAtEpisodeEnd => {
                self.sleep_deadline = None;
                self.sleep_at_episode_end = true;
                self.publish();
                Ok(true)
            }
            Command::PlayEpisode(id) => self.play_episode(id).await,
            Command::RestoreLastInProgress => self.restore_last_in_progress().await,
            Command::Shutdown => Ok(true),
        }
    }

    fn intends_to_play(&self) -> bool {
        matches!(self.phase, Phase::Loading { play_when_ready: true, .. } | Phase::Ready { playing: true })
    }

    fn current_id(&self) -> Option<i64> {
        self.current.as_ref().map(|c| c.info.id)
    }

    async fn play(&mut self) -> anyhow::Result<bool> {
        match self.phase {
            Phase::Idle => match self.current_id() {
                Some(id) => self.load_episode(id, true).await?,
                None => {
                    if !self.advance(&[], true).await? {
                        anyhow::bail!("nothing queued to play");
                    }
                }
            },
            Phase::Loading { ref mut play_when_ready, .. } => *play_when_ready = true,
            Phase::Ready { playing: true } => {}
            Phase::Ready { playing: false } => {
                if let Some(paused_at) = self.paused_at_ms.take() {
                    if self.seek_in_flight.is_none() {
                        let actual = self.pipeline.position_ms();
                        if actual.is_none_or(|a| a.abs_diff(paused_at) > RESUME_DRIFT_MS) {
                            tracing::warn!("pipeline position {actual:?}ms drifted from paused position {paused_at}ms; restoring");
                            self.request_seek(paused_at)?;
                        }
                    }
                }
                self.pipeline.play()?;
                self.phase = Phase::Ready { playing: true };
                self.position_at = Instant::now();
                self.milestone(MilestoneKind::Play);
            }
        }
        self.publish();
        Ok(true)
    }

    async fn pause(&mut self) -> anyhow::Result<bool> {
        match self.phase {
            Phase::Idle | Phase::Ready { playing: false } => return Ok(false),
            Phase::Loading { ref mut play_when_ready, .. } => *play_when_ready = false,
            Phase::Ready { playing: true } => {
                self.refresh_position();
                self.pipeline.pause()?;
                self.phase = Phase::Ready { playing: false };
                self.paused_at_ms = Some(self.position_ms);
                self.persist().await;
                self.milestone(MilestoneKind::Pause);
            }
        }
        self.publish();
        Ok(true)
    }

    async fn stop(&mut self) {
        if self.current.is_none() {
            return;
        }
        self.refresh_position();
        self.persist().await;
        self.milestone(MilestoneKind::Stop);
        self.clear_loaded();
        self.publish();
    }

    async fn next(&mut self) -> anyhow::Result<bool> {
        let exclude: Vec<i64> = self.current_id().into_iter().collect();
        self.refresh_position();
        self.persist().await;
        if self.advance(&exclude, true).await? {
            Ok(true)
        } else {
            self.stop().await;
            Ok(false)
        }
    }

    async fn play_episode(&mut self, id: i64) -> anyhow::Result<bool> {
        let previous = match self.current_id() {
            Some(current) if current == id => return self.play().await,
            Some(previous) => {
                self.refresh_position();
                self.persist().await;
                Some(previous)
            }
            None => None,
        };
        self.failed.remove(&id);
        self.requested = None;
        self.load_episode(id, true).await?;
        self.requested = Some(RequestedPlay { episode_id: id, previous });
        self.apply_requested_play().await;
        Ok(true)
    }

    /// Moves the queue for a chosen episode once it's ready.
    async fn apply_requested_play(&mut self) {
        let Some(requested) = self.requested else { return };
        if !matches!(self.phase, Phase::Ready { .. }) {
            return;
        }
        self.requested = None;
        if self.current_id() != Some(requested.episode_id) {
            return;
        }
        if let Some(previous) = requested.previous {
            if let Err(e) = store::place_after_interruption(&*self.db.lock().await, previous, requested.episode_id) {
                tracing::error!("failed to reorder the queue after playing episode {}: {e}", requested.episode_id);
            }
            let _ = self.events.send(Event::LibraryChanged);
        }
    }

    async fn restore_last_in_progress(&mut self) -> anyhow::Result<bool> {
        if self.phase != Phase::Idle {
            return Ok(false);
        }
        let Some(id) = store::last_in_progress(&*self.db.lock().await)? else {
            return Ok(false);
        };
        self.load_episode(id, false).await?;
        Ok(true)
    }

    /// Loads the next queued episode, skipping `exclude` and anything that
    /// failed this session. Returns false if nothing is left.
    async fn advance(&mut self, exclude: &[i64], play: bool) -> anyhow::Result<bool> {
        loop {
            let mut skip: Vec<i64> = exclude.to_vec();
            skip.extend(self.failed.iter().copied());
            let Some(id) = store::next_episode(&*self.db.lock().await, &skip)? else {
                return Ok(false);
            };
            match self.load_episode(id, play).await {
                Ok(()) => return Ok(true),
                Err(e) => {
                    tracing::error!("skipping episode {id}: {e:#}");
                    self.failed.insert(id);
                    let file_problem = crate::is_file_missing(&e);
                    let _ = self.events.send(Event::EpisodeFailed { episode_id: id, message: format!("{e:#}"), file_problem, requested: false });
                }
            }
        }
    }

    async fn load_episode(&mut self, id: i64, play: bool) -> anyhow::Result<()> {
        let (episode, threshold, chapters) = {
            let db = self.db.lock().await;
            (store::playable_episode(&db, id)?, store::played_threshold_ms(&db), store::chapters(&db, id)?)
        };
        let episode = episode.ok_or_else(|| crate::FileMissing(format!("episode {id} has no downloaded file")))?;
        if !std::path::Path::new(&episode.path).exists() {
            return Err(crate::FileMissing(format!("episode {id}'s file is missing: {}", episode.path)).into());
        }
        let chapters = if chapters.is_empty() {
            let path = std::path::PathBuf::from(&episode.path);
            tokio::task::spawn_blocking(move || phodder_core::chapters::from_id3_file(&path)).await.unwrap_or_default()
        } else {
            chapters
        };

        if self.current_id() != Some(id) {
            self.persist().await;
        }
        self.pipeline.unload();
        // Messages from the previous stream are queued once the
        // pipeline reaches NULL. Draining them keeps a stale EOS or
        // ASYNC_DONE from acting on the new episode.
        while let Some(Some(msg)) = self.bus.next().now_or_never() {
            tracing::trace!("discarding stale bus message {:?}", msg.type_());
        }

        tracing::info!("loading episode {id} at {}ms (play={play}): {}", episode.resume_ms, episode.path);
        self.missing_decoder = None;
        self.current = Some(Loaded {
            info: episode.info,
            chapters: Arc::new(chapters),
            played_threshold_ms: threshold,
            threshold_marked: false,
            last_chapter: None,
            user_chapter: None,
        });
        self.position_ms = episode.resume_ms;
        self.position_at = Instant::now();
        self.duration_ms = episode.feed_duration_ms;
        self.seek_in_flight = None;
        self.seek_queued = false;
        self.paused_at_ms = None;
        self.phase = Phase::Loading { stage: LoadStage::Prerolling, play_when_ready: play, since: Instant::now() };

        match self.pipeline.open(&episode.path) {
            Ok(gst::StateChangeSuccess::Async) => {}
            Ok(_) => self.on_prerolled(),
            Err(e) => {
                self.clear_loaded();
                self.publish();
                return Err(e.context(format!("failed to open episode {id}")));
            }
        }
        self.publish();
        Ok(())
    }

    fn clear_loaded(&mut self) {
        self.pipeline.unload();
        self.phase = Phase::Idle;
        self.current = None;
        self.position_ms = 0;
        self.position_at = Instant::now();
        self.duration_ms = None;
        self.seek_in_flight = None;
        self.seek_queued = false;
        self.paused_at_ms = None;
    }

    /// A seek the user asked for: the chapter it lands in plays even if a
    /// skip rule matches it.
    fn user_seek(&mut self, seek: impl FnOnce(&mut Self) -> anyhow::Result<bool>) -> anyhow::Result<bool> {
        let result = seek(self);
        let index = self.chapter_index_at(self.position_ms);
        if let Some(current) = self.current.as_mut() {
            current.user_chapter = index;
        }
        result
    }

    fn chapter_index_at(&self, position_ms: u64) -> Option<usize> {
        self.current.as_ref()?.chapters.iter().rposition(|c| c.start_ms <= position_ms)
    }

    /// Skips a chapter its podcast's rules match, when playback runs into
    /// it. Resuming partway into one plays it: the listener stopped there.
    async fn check_chapter_skip(&mut self) {
        let position = self.position_ms;
        let Some(index) = self.chapter_index_at(position) else { return };
        let Some(current) = self.current.as_mut() else { return };
        let previous = current.last_chapter.replace(index);
        if previous == Some(index) || current.user_chapter == Some(index) {
            return;
        }
        if previous.is_none() && position > current.chapters[index].start_ms + 1500 {
            return;
        }
        let episode_id = current.info.id;
        let title = current.chapters[index].title.clone();
        let next_start = current.chapters.get(index + 1).map(|c| c.start_ms);
        let rules = store::chapter_skip_rules(&*self.db.lock().await, episode_id);
        if !phodder_core::filter::should_skip_chapter(&rules, &title) {
            return;
        }
        // Past the last chapter, a seek to the end finishes the episode.
        let target = next_start.or(self.duration_ms).unwrap_or(position);
        tracing::info!("skipping chapter {title:?} of episode {episode_id}: {position}ms -> {target}ms");
        if let Err(e) = self.request_seek(target) {
            tracing::warn!("chapter skip failed: {e:#}");
        }
    }

    fn chapter_starts(&self) -> Vec<u64> {
        self.current.as_ref().map(|c| c.chapters.iter().map(|ch| ch.start_ms).collect()).unwrap_or_default()
    }

    fn previous_chapter(&mut self) -> anyhow::Result<bool> {
        self.refresh_position();
        let pos = self.position_ms;
        let starts = self.chapter_starts();
        let Some(current) = starts.iter().rposition(|&s| s <= pos) else {
            return self.request_seek(0);
        };
        let target = if pos - starts[current] > CHAPTER_RESTART_THRESHOLD_MS || current == 0 {
            starts[current]
        } else {
            starts[current - 1]
        };
        self.request_seek(target)
    }

    /// No-op on the last chapter.
    fn next_chapter(&mut self) -> anyhow::Result<bool> {
        self.refresh_position();
        let pos = self.position_ms;
        match self.chapter_starts().into_iter().find(|&s| s > pos) {
            Some(target) => self.request_seek(target),
            None => Ok(false),
        }
    }

    fn seek_by(&mut self, offset_ms: i64) -> anyhow::Result<bool> {
        self.refresh_position();
        let target = (self.position_ms as i64).saturating_add(offset_ms).max(0) as u64;
        self.request_seek(target)
    }

    /// Seeks are clamped to the duration; a seek to the end finishes the
    /// episode through the normal EOS path.
    fn request_seek(&mut self, target_ms: u64) -> anyhow::Result<bool> {
        let target = match self.duration_ms {
            Some(d) => target_ms.min(d),
            None => target_ms,
        };
        match self.phase {
            Phase::Idle if self.current.is_some() => {
                self.position_ms = target;
                self.position_at = Instant::now();
            }
            Phase::Idle => anyhow::bail!("nothing loaded to seek in"),
            Phase::Loading { stage, .. } => {
                self.position_ms = target;
                if stage == LoadStage::Seeking {
                    self.seek_queued = true;
                }
            }
            Phase::Ready { .. } => {
                self.position_ms = target;
                self.position_at = Instant::now();
                self.paused_at_ms = None;
                if self.seek_in_flight.is_some() {
                    self.seek_queued = true;
                } else {
                    self.issue_seek();
                }
            }
        }
        self.publish();
        Ok(true)
    }

    fn issue_seek(&mut self) {
        self.seek_queued = false;
        if self.pipeline.seek(self.position_ms, self.rate) {
            self.seek_in_flight = Some(Instant::now());
        } else {
            tracing::warn!("pipeline rejected seek to {}ms", self.position_ms);
            self.seek_in_flight = None;
        }
    }

    async fn set_rate(&mut self, rate: f64) -> anyhow::Result<bool> {
        if !RATE_RANGE.contains(&rate) {
            anyhow::bail!("playback rate {rate} outside {RATE_RANGE:?}");
        }
        self.rate = rate;
        store::save_rate(&*self.db.lock().await, rate)?;
        match self.phase {
            Phase::Ready { .. } => {
                self.refresh_position();
                self.request_seek(self.position_ms)?;
            }
            Phase::Loading { stage: LoadStage::Seeking, .. } => self.seek_queued = true,
            _ => {}
        }
        self.publish();
        Ok(true)
    }

    /// Updates `position_ms` from the pipeline when it can be trusted.
    fn refresh_position(&mut self) {
        if !matches!(self.phase, Phase::Ready { .. }) || self.seek_in_flight.is_some() {
            return;
        }
        if let Some(p) = self.pipeline.position_ms() {
            self.position_ms = p;
            self.position_at = Instant::now();
        }
    }

    async fn on_bus(&mut self, msg: gst::Message) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(_) => {
                if matches!(self.phase, Phase::Ready { .. }) {
                    self.on_eos().await;
                }
            }
            MessageView::AsyncDone(_) if self.pipeline.is_playbin(msg.src()) => {
                self.on_async_done().await;
                self.apply_requested_play().await;
            }
            MessageView::Error(err) => {
                let message = match err.debug() {
                    Some(debug) => format!("{} ({debug})", err.error()),
                    None => err.error().to_string(),
                };
                let error = err.error();
                let not_media = |t: &String| !t.starts_with("audio/") && !t.starts_with("video/");
                let file_problem = error.kind::<gst::StreamError>().is_some_and(|k| k != gst::StreamError::CodecNotFound)
                    || error.kind::<gst::ResourceError>().is_some_and(|k| {
                        matches!(k, gst::ResourceError::NotFound | gst::ResourceError::OpenRead | gst::ResourceError::Read)
                    })
                    || (error.matches(gst::CoreError::MissingPlugin) && self.missing_decoder.as_ref().is_some_and(not_media));
                self.on_error(message, file_problem).await;
            }
            MessageView::Warning(w) => tracing::warn!("GStreamer warning: {}", w.error()),
            MessageView::Element(e) => {
                if let Some(s) = e.structure().filter(|s| s.name() == "missing-plugin") {
                    if let Some(media_type) = s.get::<gst::Caps>("detail").ok().and_then(|caps| caps.structure(0).map(|s| s.name().to_string())) {
                        self.missing_decoder = Some(media_type);
                    }
                }
            }
            MessageView::Toc(toc) => self.on_toc(&toc.toc().0),
            MessageView::DurationChanged(_) => {
                if let Some(d) = self.pipeline.duration_ms() {
                    self.duration_ms = Some(d);
                    self.publish();
                }
            }
            _ => {}
        }
    }

    /// Chapters embedded in the file are used only when none are stored.
    fn on_toc(&mut self, toc: &gst::Toc) {
        let Some(current) = self.current.as_mut() else { return };
        if !current.chapters.is_empty() {
            return;
        }
        let mut chapters = Vec::new();
        collect_toc_chapters(&toc.entries(), &mut chapters);
        if chapters.is_empty() {
            return;
        }
        chapters.sort_by_key(|c| c.start_ms);
        tracing::debug!("{} chapters embedded in episode {}", chapters.len(), current.info.id);
        current.chapters = Arc::new(chapters);
        self.publish();
    }

    async fn on_async_done(&mut self) {
        match self.phase {
            Phase::Idle => {}
            Phase::Loading { stage: LoadStage::Prerolling, .. } => self.on_prerolled(),
            Phase::Loading { stage: LoadStage::Seeking, .. } => {
                if self.seek_queued && self.pipeline.seek(self.position_ms, self.rate) {
                    self.seek_queued = false;
                    return;
                }
                self.finish_loading();
            }
            Phase::Ready { .. } => {
                if self.seek_in_flight.take().is_none() {
                    return;
                }
                if self.seek_queued {
                    self.issue_seek();
                    return;
                }
                self.refresh_position();
                let _ = self.events.send(Event::Seeked { position_ms: self.position_ms });
                self.milestone(MilestoneKind::Seek);
                self.persist().await;
                self.publish();
            }
        }
    }

    fn on_prerolled(&mut self) {
        if let Some(d) = self.pipeline.duration_ms() {
            self.duration_ms = Some(d);
        }
        if let Some(d) = self.duration_ms {
            self.position_ms = self.position_ms.min(d);
        }
        // The rate only takes effect through a seek, so one is needed even
        // at position 0 when the rate isn't 1.0.
        if self.position_ms > 0 || self.rate != 1.0 {
            if self.pipeline.seek(self.position_ms, self.rate) {
                self.seek_queued = false;
                if let Phase::Loading { ref mut stage, .. } = self.phase {
                    *stage = LoadStage::Seeking;
                }
                return;
            }
            tracing::warn!("initial seek to {}ms at rate {} rejected; starting from 0", self.position_ms, self.rate);
            self.position_ms = 0;
        }
        self.finish_loading();
    }

    fn finish_loading(&mut self) {
        let Phase::Loading { play_when_ready, .. } = self.phase else { return };
        self.phase = Phase::Ready { playing: false };
        self.position_at = Instant::now();
        if play_when_ready {
            match self.pipeline.play() {
                Ok(()) => {
                    self.phase = Phase::Ready { playing: true };
                    self.milestone(MilestoneKind::Play);
                }
                Err(e) => tracing::error!("failed to start playback: {e}"),
            }
        }
        if let Some(id) = self.current_id() {
            if self.last_error_retry.is_some_and(|(eid, _)| eid != id) {
                self.last_error_retry = None;
            }
        }
        self.publish();
    }

    async fn on_eos(&mut self) {
        let Some(id) = self.current_id() else { return };
        tracing::info!("episode {id} finished");
        let finished_at = self.duration_ms.unwrap_or(self.position_ms);
        if let Err(e) = store::mark_finished(&*self.db.lock().await, id) {
            tracing::error!("failed to mark episode {id} finished: {e}");
        }
        let _ = self.events.send(Event::Milestone { episode_id: id, position_ms: finished_at, kind: MilestoneKind::Finished });
        let _ = self.events.send(Event::LibraryChanged);

        // Cleared before advancing so the finished episode's reset
        // position isn't overwritten by a later persist.
        self.clear_loaded();
        // An end-of-episode sleep timer fires here: the next episode loads
        // paused, ready to resume.
        let play_next = !std::mem::take(&mut self.sleep_at_episode_end);
        match self.advance(&[id], play_next).await {
            Ok(true) => {}
            Ok(false) => tracing::info!("queue empty; stopping"),
            Err(e) => tracing::error!("failed to advance after episode {id}: {e:#}"),
        }
        self.publish();
    }

    async fn on_error(&mut self, message: String, file_problem: bool) {
        tracing::error!("pipeline error: {message}");
        let Some(id) = self.current_id() else { return };
        match self.phase {
            Phase::Idle => {}
            Phase::Loading { .. } if !file_problem && self.recently_errored(id) => {
                tracing::warn!("reloading episode {id} after an error failed too; holding it until play is pressed");
                self.hold_loaded().await;
            }
            Phase::Loading { play_when_ready, .. } => {
                // The file can't be played; skip it for the rest of the session.
                self.failed.insert(id);
                let requested = self.requested.take().filter(|r| r.episode_id == id);
                let _ = self.events.send(Event::EpisodeFailed { episode_id: id, message, file_problem, requested: requested.is_some() });
                self.clear_loaded();
                if let Some(requested) = requested {
                    // Back to what was loaded, paused, rather than something
                    // the user didn't pick.
                    if let Some(previous) = requested.previous {
                        if let Err(e) = self.load_episode(previous, false).await {
                            tracing::error!("failed to reload episode {previous} after episode {id} failed: {e:#}");
                        }
                    }
                } else if play_when_ready {
                    if let Err(e) = self.advance(&[id], true).await {
                        tracing::error!("failed to advance past episode {id}: {e:#}");
                    }
                }
                self.publish();
            }
            Phase::Ready { playing } => {
                // A runtime error (for example a vanished audio output) gets
                // one reload at the last known position.
                let can_retry = self.last_error_retry.is_none_or(|(eid, at)| eid != id || at.elapsed() > ERROR_RETRY_WINDOW);
                self.persist().await;
                if file_problem && !can_retry {
                    let _ = self.events.send(Event::EpisodeFailed { episode_id: id, message, file_problem: false, requested: false });
                    self.stop().await;
                } else if can_retry {
                    self.last_error_retry = Some((id, Instant::now()));
                    if let Err(e) = self.load_episode(id, playing).await {
                        tracing::error!("failed to reload episode {id} after error: {e:#}");
                        if crate::is_file_missing(&e) {
                            self.clear_loaded();
                            self.publish();
                        } else {
                            self.hold_loaded().await;
                        }
                    }
                } else {
                    tracing::warn!("episode {id} failed again within {ERROR_RETRY_WINDOW:?}; holding it until play is pressed");
                    self.hold_loaded().await;
                }
            }
        }
    }

    /// True within ERROR_RETRY_WINDOW of a runtime error in `id`, when a load
    /// of it is the reload that error started.
    fn recently_errored(&self, id: i64) -> bool {
        self.last_error_retry.is_some_and(|(eid, at)| eid == id && at.elapsed() <= ERROR_RETRY_WINDOW)
    }

    /// Takes the pipeline down but keeps the episode and position, paused,
    /// so the player still shows it; play reloads it. For errors from the
    /// audio output, such as a Bluetooth device going away mid-reload.
    async fn hold_loaded(&mut self) {
        self.pipeline.unload();
        self.phase = Phase::Idle;
        self.position_at = Instant::now();
        self.seek_in_flight = None;
        self.seek_queued = false;
        self.paused_at_ms = None;
        self.persist().await;
        self.publish();
    }

    async fn on_tick(&mut self) {
        if let Phase::Loading { since, .. } = self.phase {
            if since.elapsed() > LOAD_TIMEOUT {
                self.on_error("timed out loading episode".into(), false).await;
                return;
            }
        }
        if self.seek_in_flight.is_some_and(|t| t.elapsed() > SEEK_TIMEOUT) {
            tracing::warn!("seek to {}ms never completed", self.position_ms);
            self.seek_in_flight = None;
            if self.seek_queued {
                self.issue_seek();
            }
        }
        if self.phase != (Phase::Ready { playing: true }) {
            return;
        }
        self.refresh_position();
        self.check_chapter_skip().await;
        self.check_played_threshold().await;
        if self.last_persist.elapsed() >= PERSIST_INTERVAL {
            self.persist().await;
        }
        if self.last_publish.elapsed() >= PUBLISH_INTERVAL {
            self.publish();
        }
    }

    async fn check_played_threshold(&mut self) {
        let (Some(current), Some(duration)) = (self.current.as_mut(), self.duration_ms) else { return };
        let Some(threshold) = current.played_threshold_ms else { return };
        if current.threshold_marked || duration.saturating_sub(self.position_ms) > threshold {
            return;
        }
        current.threshold_marked = true;
        let id = current.info.id;
        match store::mark_played(&*self.db.lock().await, id) {
            Ok(_) => {
                let _ = self.events.send(Event::Milestone { episode_id: id, position_ms: duration, kind: MilestoneKind::Finished });
                let _ = self.events.send(Event::LibraryChanged);
            }
            Err(e) => tracing::error!("failed to mark episode {id} played: {e}"),
        }
    }

    async fn on_sleep_timer(&mut self) {
        self.sleep_deadline = None;
        if let Err(e) = self.pause().await {
            tracing::error!("sleep timer failed to pause: {e:#}");
        }
        self.publish();
    }

    async fn persist(&mut self) {
        self.last_persist = Instant::now();
        let Some(id) = self.current_id() else { return };
        if let Err(e) = store::save_position(&*self.db.lock().await, id, self.position_ms) {
            tracing::error!("failed to save position for episode {id}: {e}");
        }
    }

    fn milestone(&self, kind: MilestoneKind) {
        if let Some(id) = self.current_id() {
            let _ = self.events.send(Event::Milestone { episode_id: id, position_ms: self.position_ms, kind });
        }
    }

    fn publish(&mut self) {
        self.last_publish = Instant::now();
        let status = match self.phase {
            Phase::Idle if self.current.is_some() => Status::Paused,
            Phase::Idle => Status::Stopped,
            Phase::Loading { play_when_ready: true, .. } | Phase::Ready { playing: true } => Status::Playing,
            Phase::Loading { .. } | Phase::Ready { playing: false } => Status::Paused,
        };
        self.snapshot_tx.send_replace(Snapshot {
            status,
            episode: self.current.as_ref().map(|c| c.info.clone()),
            position_ms: self.position_ms,
            position_at: self.position_at,
            duration_ms: self.duration_ms,
            rate: self.rate,
            sleep_deadline: self.sleep_deadline,
            sleep_at_episode_end: self.sleep_at_episode_end,
            loading: matches!(self.phase, Phase::Loading { .. }) || self.seek_in_flight.is_some(),
            chapters: self.current.as_ref().map(|c| c.chapters.clone()).unwrap_or_default(),
        });
    }
}

/// Editions (ID3 CTOC) nest their chapters; only chapter entries count.
fn collect_toc_chapters(entries: &[gst::TocEntry], out: &mut Vec<Chapter>) {
    for entry in entries {
        if entry.entry_type() == gst::TocEntryType::Chapter {
            if let Some((start, _)) = entry.start_stop_times() {
                let title = entry
                    .tags()
                    .and_then(|tags| tags.get::<gst::tags::Title>().map(|t| t.get().to_string()))
                    .unwrap_or_default();
                out.push(Chapter { start_ms: (start.max(0) as u64) / 1_000_000, title, image_url: None, url: None });
            }
        }
        collect_toc_chapters(&entry.sub_entries(), out);
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d.into()).await,
        None => std::future::pending().await,
    }
}
