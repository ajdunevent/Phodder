//! End-to-end tests against real MP3 files played through a clock-synced
//! fakesink, so timing behaves as it would on a real output.

use phodder_playback::gstreamer as gst;
use phodder_playback::{Config, Event, MilestoneKind, Player, Snapshot, Status};
use gst::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

struct Fixture {
    dir: tempfile::TempDir,
    db: Arc<Mutex<rusqlite::Connection>>,
    sub_id: i64,
    next_queue_pos: f64,
}

impl Fixture {
    fn new() -> Self {
        let _ = tracing_subscriber::fmt().with_env_filter("phodder_playback=debug").with_test_writer().try_init();
        let dir = tempfile::tempdir().unwrap();
        let conn = phodder_core::open_connection(&dir.path().join("test.db")).unwrap();
        phodder_core::db::migrate(&conn).unwrap();
        conn.execute("INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/feed', 'Show', 1)", [])
            .unwrap();
        let sub_id = conn.last_insert_rowid();
        Self { dir, db: Arc::new(Mutex::new(conn)), sub_id, next_queue_pos: 1.0 }
    }

    fn player(&self) -> Player {
        let sink = gst::ElementFactory::make("fakesink").property("sync", true).build().unwrap();
        Player::spawn(Config { db: self.db.clone(), audio_sink: Some(sink) }).unwrap()
    }

    fn audio_file(&self, name: &str, seconds: u32) -> PathBuf {
        self.audio_file_in_format(name, seconds, 44_100, 2)
    }

    fn audio_file_in_format(&self, name: &str, seconds: u32, sample_rate: u32, channels: u32) -> PathBuf {
        let path = self.dir.path().join(name);
        encode_mp3_in_format(&path, seconds, sample_rate, channels);
        path
    }

    /// Inserts a downloaded episode; `queued` appends it to the queue.
    async fn episode(&mut self, title: &str, path: &Path, queued: bool) -> i64 {
        let queue_pos = queued.then(|| {
            let p = self.next_queue_pos;
            self.next_queue_pos += 1.0;
            p
        });
        let db = self.db.lock().await;
        db.execute(
            "INSERT INTO episodes (subscription_id, guid, title, audio_url, publish_date, download_status, local_file_path, group_queue_position)
             VALUES (?1, ?2, ?2, 'https://example.com/a.mp3', '2026-01-01T00:00:00Z', 'downloaded', ?3, ?4)",
            rusqlite::params![self.sub_id, title, path.to_str().unwrap(), queue_pos],
        )
        .unwrap();
        db.last_insert_rowid()
    }

    async fn row(&self, id: i64) -> (bool, i64, Option<f64>) {
        self.db
            .lock()
            .await
            .query_row(
                "SELECT played, playback_position_ms, group_queue_position FROM episodes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    async fn set(&self, key: &str, value: &str) {
        phodder_core::settings::set_setting(&*self.db.lock().await, key, value).unwrap();
    }
}

fn encode_mp3_in_format(path: &Path, seconds: u32, sample_rate: u32, channels: u32) {
    gst::init().unwrap();
    let buffers = seconds * sample_rate / 1024;
    let pipeline = gst::parse::launch(&format!(
        "audiotestsrc num-buffers={buffers} samplesperbuffer=1024 ! audio/x-raw,rate={sample_rate},channels={channels} \
         ! audioconvert ! lamemp3enc target=bitrate bitrate=64 cbr=true ! filesink location=\"{}\"",
        path.display()
    ))
    .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus.timed_pop_filtered(gst::ClockTime::from_seconds(30), &[gst::MessageType::Eos, gst::MessageType::Error]).unwrap();
    assert_eq!(msg.type_(), gst::MessageType::Eos, "encoding failed: {msg:?}");
    pipeline.set_state(gst::State::Null).unwrap();
}

async fn wait_for(player: &Player, what: &str, timeout_s: u64, pred: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let mut rx = player.watch();
    let result = tokio::time::timeout(Duration::from_secs(timeout_s), async {
        loop {
            let snap = rx.borrow_and_update().clone();
            if pred(&snap) {
                return snap;
            }
            rx.changed().await.unwrap();
        }
    })
    .await;
    match result {
        Ok(s) => s,
        Err(_) => panic!("timed out waiting for {what}; last snapshot: {:?}", player.snapshot()),
    }
}

fn playing(id: i64) -> impl Fn(&Snapshot) -> bool {
    move |s| s.status == Status::Playing && !s.loading && s.episode.as_ref().is_some_and(|e| e.id == id)
}

async fn settle(player: &Player) -> Snapshot {
    wait_for(player, "seek to settle", 5, |s| !s.loading).await
}

fn near(actual: u64, expected: u64, tolerance: u64) -> bool {
    actual.abs_diff(expected) <= tolerance
}

#[tokio::test]
async fn natural_end_advances_marks_played_and_dequeues() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 2), true).await;
    let b = f.episode("B", &f.audio_file("b.mp3", 2), true).await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    wait_for(&player, "advance to B", 6, playing(b)).await;
    assert_eq!(f.row(a).await, (true, 0, None), "A should be played, reset, and dequeued");
    let in_history = phodder_core::subscriptions::list_history_episodes(&*f.db.lock().await).unwrap();
    assert_eq!(in_history.iter().map(|e| e.id).collect::<Vec<_>>(), vec![a], "finished episode is in History");

    wait_for(&player, "stop after B", 6, |s| s.status == Status::Stopped).await;
    assert!(f.row(b).await.0);
    assert!(player.snapshot().episode.is_none());
}

#[tokio::test]
async fn already_played_queued_episode_does_not_repeat() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 2), true).await;
    f.db.lock().await.execute("UPDATE episodes SET played = 1 WHERE id = ?1", [a]).unwrap();
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    wait_for(&player, "stop", 6, |s| s.status == Status::Stopped).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(player.status(), Status::Stopped);
    assert_eq!(f.row(a).await.2, None);
}

#[tokio::test]
async fn next_skips_the_current_episode_and_keeps_it_queued() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 10), true).await;
    let b = f.episode("B", &f.audio_file("b.mp3", 10), true).await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    assert!(player.next().await.unwrap());
    wait_for(&player, "B playing", 5, playing(b)).await;
    let (played, _, queued) = f.row(a).await;
    assert!(!played && queued.is_some(), "skipped episode stays unplayed and queued");

    // B is now the one skipped; A, still queued, comes next.
    assert!(player.next().await.unwrap());
    wait_for(&player, "A playing again", 5, playing(a)).await;
}

#[tokio::test]
async fn skip_while_paused_survives_resume() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), true).await;
    f.set("skip_forward_ms", "5000").await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    player.pause().await.unwrap();
    let before = player.snapshot().position_ms;
    player.skip_forward().await.unwrap();
    let after_skip = settle(&player).await.position_ms;
    assert!(near(after_skip, before + 5000, 300), "skip moved {before} -> {after_skip}");
    assert!(near(f.row(a).await.1 as u64, after_skip, 300), "skip position persisted");

    player.play().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let now = player.snapshot().position_now_ms();
    assert!(now >= after_skip + 1000 && now < after_skip + 2500, "resume kept the skip: {after_skip} -> {now}");
}

#[tokio::test]
async fn rapid_skips_accumulate() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 30), true).await;
    f.set("skip_forward_ms", "4000").await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    player.pause().await.unwrap();
    let start = player.snapshot().position_ms;
    for _ in 0..4 {
        player.send_skip_forward();
    }
    player.skip_backward().await.unwrap();
    let end = settle(&player).await.position_ms;
    assert!(near(end, start + 16_000 - 10_000, 300), "{start} -> {end}");
}

#[tokio::test]
async fn rate_applies_to_new_episodes_and_survives_seeks() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), true).await;
    let b = f.episode("B", &f.audio_file("b.mp3", 20), true).await;
    let player = f.player();

    let measure = |player: Player| async move {
        let p0 = player.snapshot().position_now_ms();
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let s1 = player.snapshot();
        eprintln!("measured from {p0}: {s1:?}");
        s1.position_now_ms() as i64 - p0 as i64
    };

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    player.set_rate(2.0).await.unwrap();
    settle(&player).await;
    let advanced = measure(player.clone()).await;
    assert!((3400..=4600).contains(&advanced), "2x rate on A advanced {advanced}ms in 2s");

    player.seek_to(1000).await.unwrap();
    settle(&player).await;
    let advanced = measure(player.clone()).await;
    assert!((3400..=4600).contains(&advanced), "after seek advanced {advanced}ms in 2s");

    player.next().await.unwrap();
    wait_for(&player, "B playing", 5, playing(b)).await;
    let advanced = measure(player.clone()).await;
    assert!((3400..=4600).contains(&advanced), "2x rate on B advanced {advanced}ms in 2s");
    assert_eq!(player.snapshot().rate, 2.0);

    // Real pipeline position, not only the extrapolated value.
    let saved = {
        player.pause().await.unwrap();
        f.row(b).await.1 as u64
    };
    assert!(saved >= 4000, "pipeline position at 2x after ~2s was {saved}");

    let restarted = f.player();
    assert_eq!(wait_for(&restarted, "rate loaded", 2, |s| s.rate == 2.0).await.rate, 2.0);
}

#[tokio::test]
async fn position_is_saved_on_pause_and_restored_on_startup() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), true).await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    player.seek_to(7000).await.unwrap();
    settle(&player).await;
    player.pause().await.unwrap();
    let saved = f.row(a).await.1 as u64;
    assert!(near(saved, 7000, 400), "saved {saved}");
    player.shutdown().await.unwrap();

    let player = f.player();
    player.restore_last_in_progress().await.unwrap();
    let snap = wait_for(&player, "restored paused", 5, |s| s.status == Status::Paused && !s.loading).await;
    assert_eq!(snap.episode.unwrap().id, a);
    assert!(near(snap.position_ms, saved, 300), "restored at {}", snap.position_ms);
    player.play().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(player.snapshot().position_now_ms() > saved, "resumed forward from the saved position");
}

#[tokio::test]
async fn playing_another_episode_resumes_it_and_requeues_the_interrupted_one() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 45), true).await;
    let b = f.episode("B", &f.audio_file("b.mp3", 20), true).await;
    let c = f.episode("C", &f.audio_file("c.mp3", 20), false).await;
    f.db.lock().await.execute("UPDATE episodes SET playback_position_ms = 9000 WHERE id = ?1", [c]).unwrap();
    // Past the started threshold, so A counts as in progress.
    f.db.lock().await.execute("UPDATE episodes SET playback_position_ms = 35000 WHERE id = ?1", [a]).unwrap();
    // B ahead of A in the queue, so A's return to the front is observable.
    f.db.lock().await.execute("UPDATE episodes SET group_queue_position = 0.5 WHERE id = ?1", [b]).unwrap();
    let player = f.player();

    player.play_episode(a).await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    player.play_episode(c).await.unwrap();
    let snap = wait_for(&player, "C playing", 5, playing(c)).await;
    assert!(near(snap.position_ms, 9000, 300), "C resumed at {}", snap.position_ms);

    let next = phodder_core::queue::next_episode(&*f.db.lock().await).unwrap().unwrap();
    assert_eq!(next.episode_id, a, "interrupted episode returns to the front of its group");
}

#[tokio::test]
async fn sleep_timer_pauses_and_keeps_position() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), true).await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    player.set_sleep_timer(Some(Duration::from_millis(1500))).await.unwrap();
    let snap = wait_for(&player, "sleep timer pause", 4, |s| s.status == Status::Paused).await;
    assert!(snap.sleep_deadline.is_none());
    assert!(snap.position_ms >= 1000, "paused at {}", snap.position_ms);
    assert!(near(f.row(a).await.1 as u64, snap.position_ms, 300));
}

#[tokio::test]
async fn unplayable_file_is_skipped() {
    let mut f = Fixture::new();
    let bad_path = f.dir.path().join("bad.mp3");
    std::fs::write(&bad_path, b"this is not audio").unwrap();
    let bad = f.episode("Bad", &bad_path, true).await;
    let good = f.episode("Good", &f.audio_file("good.mp3", 5), true).await;
    let player = f.player();
    let mut events = player.subscribe();

    player.play().await.unwrap();
    wait_for(&player, "good playing", 6, playing(good)).await;
    let mut failed = false;
    while let Ok(e) = events.try_recv() {
        failed |= matches!(e, Event::EpisodeFailed { episode_id, file_problem: true, .. } if episode_id == bad);
    }
    assert!(failed);
    assert!(!f.row(bad).await.0, "a failed episode isn't marked played");
}

#[tokio::test]
async fn special_characters_in_file_names_play() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("Episode #12 100% [live] & more.mp3", 2), true).await;
    let player = f.player();
    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
}

#[tokio::test]
async fn seek_to_end_finishes_the_episode() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), true).await;
    let player = f.player();
    let mut events = player.subscribe();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    player.seek_to(60_000).await.unwrap();
    wait_for(&player, "stop", 5, |s| s.status == Status::Stopped).await;
    assert!(f.row(a).await.0);
    let mut finished = false;
    while let Ok(e) = events.try_recv() {
        finished |= matches!(e, Event::Milestone { kind: MilestoneKind::Finished, .. });
    }
    assert!(finished);
}

#[tokio::test]
async fn played_threshold_marks_played_before_the_end() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 10), true).await;
    f.set("default_played_threshold_seconds", "5").await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    player.seek_to(6000).await.unwrap();
    settle(&player).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (played, _, queued) = f.row(a).await;
    assert!(played && queued.is_none());
    assert_eq!(player.status(), Status::Playing, "threshold doesn't interrupt playback");
}

#[tokio::test]
async fn missing_file_is_skipped() {
    let mut f = Fixture::new();
    let gone = f.episode("Gone", &f.dir.path().join("deleted.mp3"), true).await;
    let good = f.episode("Good", &f.audio_file("good.mp3", 5), true).await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "good playing", 5, playing(good)).await;
    assert!(!f.row(gone).await.0);
}

/// ID3v2.3 tag with a CTOC and one CHAP frame per `(start_ms, end_ms, title)`.
fn id3_chapter_tag(chapters: &[(u32, u32, &str)]) -> Vec<u8> {
    fn frame(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        f.extend_from_slice(&(body.len() as u32).to_be_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(body);
        f
    }
    let mut frames = Vec::new();
    let mut ctoc = b"toc\0".to_vec();
    ctoc.push(0x03); // top-level, ordered
    ctoc.push(chapters.len() as u8);
    for i in 0..chapters.len() {
        ctoc.extend_from_slice(format!("ch{i}\0").as_bytes());
    }
    frames.extend(frame(b"CTOC", &ctoc));
    for (i, (start, end, title)) in chapters.iter().enumerate() {
        let mut body = format!("ch{i}\0").into_bytes();
        body.extend_from_slice(&start.to_be_bytes());
        body.extend_from_slice(&end.to_be_bytes());
        body.extend_from_slice(&[0xff; 8]);
        let mut tit2 = vec![0u8];
        tit2.extend_from_slice(title.as_bytes());
        body.extend(frame(b"TIT2", &tit2));
        frames.extend(frame(b"CHAP", &body));
    }
    let size = frames.len() as u32;
    let syncsafe = [(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f];
    let mut tag = b"ID3\x03\x00\x00".to_vec();
    tag.extend_from_slice(&syncsafe);
    tag.extend(frames);
    tag
}

#[tokio::test]
async fn stored_chapters_drive_chapter_navigation() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 30), true).await;
    let chapters: Vec<phodder_playback::Chapter> = [(0, "Intro"), (10_000, "Middle"), (20_000, "End")]
        .into_iter()
        .map(|(start_ms, title)| phodder_playback::Chapter { start_ms, title: title.into(), image_url: None, url: None })
        .collect();
    phodder_core::chapters::replace(&*f.db.lock().await, a, &chapters).unwrap();
    let player = f.player();

    player.play().await.unwrap();
    let snap = wait_for(&player, "A playing", 5, playing(a)).await;
    assert_eq!(snap.chapters.len(), 3);
    player.pause().await.unwrap();

    player.next_chapter().await.unwrap();
    assert!(near(settle(&player).await.position_ms, 10_000, 200));
    player.next_chapter().await.unwrap();
    let snap = settle(&player).await;
    assert!(near(snap.position_ms, 20_000, 200));
    assert_eq!(snap.current_chapter().unwrap().title, "End");
    player.next_chapter().await.unwrap();
    assert!(near(settle(&player).await.position_ms, 20_000, 200), "no-op on the last chapter");

    // Within the first 3s of a chapter, Previous goes to the one before.
    player.previous_chapter().await.unwrap();
    assert!(near(settle(&player).await.position_ms, 10_000, 200));
    // Further in, it restarts the current chapter.
    player.seek_to(15_000).await.unwrap();
    settle(&player).await;
    player.previous_chapter().await.unwrap();
    assert!(near(settle(&player).await.position_ms, 10_000, 200));
}

#[tokio::test]
async fn chapters_embedded_in_the_file_are_used_when_none_are_stored() {
    let mut f = Fixture::new();
    let plain = f.audio_file("plain.mp3", 12);
    let tagged = f.dir.path().join("tagged.mp3");
    let mut bytes = id3_chapter_tag(&[(0, 4000, "Cold open"), (4000, 8000, "Interview"), (8000, 12000, "Outro")]);
    bytes.extend(std::fs::read(&plain).unwrap());
    std::fs::write(&tagged, bytes).unwrap();
    let a = f.episode("A", &tagged, true).await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    let snap = wait_for(&player, "embedded chapters", 3, |s| s.chapters.len() == 3).await;
    let titles: Vec<&str> = snap.chapters.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["Cold open", "Interview", "Outro"]);
    player.next_chapter().await.unwrap();
    assert!(near(settle(&player).await.position_ms, 4000, 200));
}

#[tokio::test]
async fn stop_if_current_unloads_only_the_named_episode() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), true).await;
    let b = f.episode("B", &f.audio_file("b.mp3", 20), true).await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    assert!(!player.stop_if_current(vec![b]).await.unwrap());
    assert_eq!(player.snapshot().status, Status::Playing);

    assert!(player.stop_if_current(vec![b, a]).await.unwrap());
    let snap = player.snapshot();
    assert_eq!(snap.status, Status::Stopped);
    assert!(snap.episode.is_none());
}

#[tokio::test]
async fn restoring_leaves_the_position_timestamp_alone() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), true).await;
    {
        let db = f.db.lock().await;
        db.execute(
            "UPDATE episodes SET playback_position_ms = 6000, position_updated_at = '2026-01-01T00:00:00+00:00' WHERE id = ?1",
            [a],
        )
        .unwrap();
    }
    let player = f.player();
    player.restore_last_in_progress().await.unwrap();
    wait_for(&player, "restored paused", 5, |s| s.status == Status::Paused && !s.loading).await;
    player.shutdown().await.unwrap();

    let stamp: String =
        f.db.lock().await.query_row("SELECT position_updated_at FROM episodes WHERE id = ?1", [a], |r| r.get(0)).unwrap();
    assert_eq!(stamp, "2026-01-01T00:00:00+00:00", "an unchanged position must not look like newer local progress to sync");
}

#[tokio::test]
async fn a_dequeued_in_progress_episode_is_not_restored() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), false).await;
    f.db.lock().await.execute("UPDATE episodes SET playback_position_ms = 6000 WHERE id = ?1", [a]).unwrap();
    let player = f.player();
    player.restore_last_in_progress().await.unwrap();
    assert!(player.snapshot().episode.is_none());
}

async fn chapters_with_skip_rules(f: &mut Fixture, episode: i64) {
    let chapters: Vec<phodder_playback::Chapter> = [(0, "Intro"), (2_000, "Sponsor: Acme"), (4_000, "Ad break"), (6_000, "Talk")]
        .into_iter()
        .map(|(start_ms, title)| phodder_playback::Chapter { start_ms, title: title.into(), image_url: None, url: None })
        .collect();
    let db = f.db.lock().await;
    phodder_core::chapters::replace(&db, episode, &chapters).unwrap();
    db.execute(
        r#"UPDATE subscriptions SET chapter_skip_rules = '[{"pattern":"sponsor","target":"title","action":"block"},{"pattern":"ad*","target":"title","action":"block"}]'"#,
        [],
    )
    .unwrap();
}

#[tokio::test]
async fn matching_chapters_are_skipped_in_a_row() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), true).await;
    chapters_with_skip_rules(&mut f, a).await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    let started = std::time::Instant::now();
    wait_for(&player, "the Talk chapter", 8, |s| s.current_chapter().is_some_and(|c| c.title == "Talk")).await;
    // Four seconds of skippable chapters, reached about two seconds in.
    assert!(started.elapsed() < Duration::from_millis(4_000), "took {:?}", started.elapsed());
}

#[tokio::test]
async fn a_chapter_the_user_seeks_into_is_not_skipped() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), true).await;
    chapters_with_skip_rules(&mut f, a).await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    player.seek_to(2_500).await.unwrap();
    settle(&player).await;
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let snap = player.snapshot();
    assert_eq!(snap.current_chapter().map(|c| c.title.as_str()), Some("Sponsor: Acme"), "position {}", snap.position_now_ms());
}

#[tokio::test]
async fn sleep_at_episode_end_loads_the_next_episode_paused() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 3), true).await;
    let b = f.episode("B", &f.audio_file("b.mp3", 20), true).await;
    let player = f.player();

    player.play().await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    player.sleep_at_episode_end().await.unwrap();
    assert!(player.snapshot().sleep_at_episode_end);

    let snap = wait_for(&player, "B loaded", 10, |s| s.episode.as_ref().is_some_and(|e| e.id == b) && !s.loading).await;
    assert_eq!(snap.status, Status::Paused, "the timer stops playback at the episode boundary");
    assert!(!snap.sleep_at_episode_end, "the timer is used up");
    assert!(f.row(a).await.0, "A was still marked played");
}

/// A crash here takes the whole test binary down.
#[tokio::test]
async fn episodes_in_different_formats_play_at_double_speed() {
    let mut f = Fixture::new();
    let stereo = f.audio_file_in_format("stereo.mp3", 20, 44_100, 2);
    let mono = f.audio_file_in_format("mono.mp3", 20, 32_000, 1);
    let a = f.episode("Stereo", &stereo, true).await;
    let b = f.episode("Mono", &mono, true).await;
    let player = f.player();
    player.set_rate(2.0).await.unwrap();
    for round in 0..12 {
        let id = if round % 2 == 0 { a } else { b };
        player.play_episode(id).await.unwrap();
        wait_for(&player, "the episode to play", 10, playing(id)).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[tokio::test]
async fn an_interrupted_episode_barely_started_keeps_its_place() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), true).await;
    let b = f.episode("B", &f.audio_file("b.mp3", 20), true).await;
    let c = f.episode("C", &f.audio_file("c.mp3", 20), true).await;
    let player = f.player();
    player.play_episode(a).await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    // The user moves A to the back and plays C.
    f.db.lock().await.execute("UPDATE episodes SET group_queue_position = 10.0 WHERE id = ?1", [a]).unwrap();
    player.play_episode(c).await.unwrap();
    wait_for(&player, "C playing", 5, playing(c)).await;
    let order: Vec<i64> = phodder_core::queue::list_queue(&*f.db.lock().await).unwrap().iter().map(|e| e.episode_id).collect();
    assert_eq!(order, [c, b, a]);
}

#[tokio::test]
async fn an_episode_that_cant_load_leaves_the_queue_and_player_alone() {
    let mut f = Fixture::new();
    let gone = f.episode("Gone", &f.dir.path().join("deleted.mp3"), true).await;
    let a = f.episode("A", &f.audio_file("a.mp3", 45), true).await;
    f.db.lock().await.execute("UPDATE episodes SET playback_position_ms = 35000 WHERE id = ?1", [a]).unwrap();
    let player = f.player();
    player.play_episode(a).await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;

    assert!(player.play_episode(gone).await.is_err());
    let order: Vec<i64> = phodder_core::queue::list_queue(&*f.db.lock().await).unwrap().iter().map(|e| e.episode_id).collect();
    assert_eq!(order, [gone, a]);
    assert_eq!(player.snapshot().episode.map(|e| e.id), Some(a));
}

#[tokio::test]
async fn a_missing_file_is_reported_as_a_file_problem() {
    let mut f = Fixture::new();
    let gone = f.episode("Gone", &f.dir.path().join("deleted.mp3"), true).await;
    let player = f.player();
    let error = player.play_episode(gone).await.unwrap_err();
    assert!(phodder_playback::is_file_missing(&error), "{error:#}");
}

#[tokio::test]
async fn a_chosen_episode_that_wont_decode_goes_back_to_the_previous_one() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 45), true).await;
    let bad_path = f.dir.path().join("bad.mp3");
    std::fs::write(&bad_path, b"this is not audio").unwrap();
    let bad = f.episode("Bad", &bad_path, true).await;
    let c = f.episode("C", &f.audio_file("c.mp3", 20), true).await;
    f.db.lock().await.execute("UPDATE episodes SET playback_position_ms = 35000 WHERE id = ?1", [a]).unwrap();
    let player = f.player();
    player.play_episode(a).await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    let mut events = player.subscribe();

    player.play_episode(bad).await.unwrap();
    let snap = wait_for(&player, "A back, paused", 6, |s| {
        s.status == Status::Paused && !s.loading && s.episode.as_ref().is_some_and(|e| e.id == a)
    })
    .await;
    assert!(snap.position_ms >= 35000, "A kept its place: {}", snap.position_ms);
    let mut reported = false;
    while let Ok(e) = events.try_recv() {
        reported |= matches!(e, Event::EpisodeFailed { episode_id, file_problem: true, requested: true, .. } if episode_id == bad);
    }
    assert!(reported);
    let order: Vec<i64> = phodder_core::queue::list_queue(&*f.db.lock().await).unwrap().iter().map(|e| e.episode_id).collect();
    assert_eq!(order, [a, bad, c], "the queue is unchanged");
}

/// An HTML error page saved in place of the audio is reported as a missing
/// text/html decoder, which a fresh download can fix.
#[tokio::test]
async fn a_saved_web_page_is_reported_as_a_file_problem() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 20), true).await;
    let page = f.dir.path().join("page.mp3");
    std::fs::write(&page, b"<!DOCTYPE html><html><head><title>404 Not Found</title></head><body><h1>Not Found</h1></body></html>\n").unwrap();
    let html = f.episode("Html", &page, true).await;
    let player = f.player();
    player.play_episode(a).await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    let mut events = player.subscribe();
    player.play_episode(html).await.unwrap();
    let reported = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if let Ok(Event::EpisodeFailed { episode_id, file_problem, .. }) = events.recv().await {
                if episode_id == html {
                    return file_problem;
                }
            }
        }
    })
    .await
    .expect("the page failed to play");
    assert!(reported);
}

/// A Bluetooth device going away errors the audio output; when the reload
/// errors too, the episode must stay loaded and paused, not vanish from the
/// player.
#[tokio::test]
async fn repeated_output_errors_keep_the_episode_loaded() {
    let mut f = Fixture::new();
    let a = f.episode("A", &f.audio_file("a.mp3", 45), true).await;
    let sink = gst::ElementFactory::make("fakesink").property("sync", true).build().unwrap();
    let player = Player::spawn(Config { db: f.db.clone(), audio_sink: Some(sink.clone()) }).unwrap();
    let output_gone = |sink: &gst::Element| {
        let message = gst::message::Error::builder(gst::ResourceError::Write, "output device disconnected").src(sink).build();
        assert!(sink.post_message(message).is_ok());
    };
    player.play_episode(a).await.unwrap();
    wait_for(&player, "A playing", 5, playing(a)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    output_gone(&sink);
    wait_for(&player, "A reloaded and playing", 6, |s| playing(a)(s) && s.position_ms >= 1500).await;
    output_gone(&sink);
    let held = wait_for(&player, "A held, paused", 6, |s| s.status == Status::Paused && !s.loading).await;
    assert_eq!(held.episode.map(|e| e.id), Some(a));
    assert!(held.position_ms >= 1500, "A kept its place: {}", held.position_ms);

    player.play().await.unwrap();
    let resumed = wait_for(&player, "A playing again", 6, playing(a)).await;
    assert!(resumed.position_ms >= 1500, "A resumed in place: {}", resumed.position_ms);
}
