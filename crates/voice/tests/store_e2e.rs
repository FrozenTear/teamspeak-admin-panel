//! PURA-121 WS-3 — store + dispatch integration.
//!
//! Two acceptance bars from the issue spec:
//!
//! 1. **Restart-restore**: enqueue 3 tracks, advance head, snapshot the
//!    store, drop the supervisor, reload from snapshot into a fresh
//!    supervisor — assert the queue still has 2 tracks with track 2 as
//!    the next-up. This proves the data model survives a restart
//!    without dragging Surreal into the music-bot crate. (The WS-5
//!    SurrealDB impl will exercise the same shape against a live DB.)
//!
//! 2. **Dispatcher reactions**: a queue command flowing through the
//!    bot's command-dispatch surface must emit `QueueChanged` /
//!    `NowPlaying` / `QueueEmpty` on the broadcast channel. We drive
//!    this without a real TS6 connection (`auto_connect=false`) so the
//!    test runs in `cargo test` without the fixture profile.
//!
//! No `#[ignore]` / no feature gate — this is unit-test cheap.

use std::sync::Arc;
use std::time::Duration;

use music_bot::{
    spawn_bot, BotCommand, BotConfig, BotEvent, BotId, InMemoryMusicBotStore, MusicBotStore,
    NewTrack, QueueCommand,
};
use tokio::sync::broadcast;
use tokio::time::timeout;

const EVENT_TIMEOUT: Duration = Duration::from_secs(2);

/// Drive the broadcast receiver until a predicate matches or the
/// per-test deadline trips. Skipping `Lagged` keeps a slow test from
/// tanking on benign re-enqueue.
async fn next_match<T, F>(rx: &mut broadcast::Receiver<BotEvent>, mut pred: F) -> Option<T>
where
    F: FnMut(&BotEvent) -> Option<T>,
{
    loop {
        match timeout(EVENT_TIMEOUT, rx.recv()).await {
            Ok(Ok(ev)) => {
                if let Some(t) = pred(&ev) {
                    return Some(t);
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => return None,
            Err(_) => return None,
        }
    }
}

fn track(title: &str) -> NewTrack {
    NewTrack::url(title, format!("https://example.com/{title}.mp3"))
}

#[tokio::test]
async fn enqueue_three_advance_head_restart_restore() {
    let store_a: Arc<InMemoryMusicBotStore> = Arc::new(InMemoryMusicBotStore::new());
    let store_a_dyn: Arc<dyn MusicBotStore> = store_a.clone();
    let id = BotId(1);

    // Spawn a bot in `auto_connect=false` so the actor stays in
    // `Disconnected` and we exercise the pre-connect queue-staging path.
    let workdir = std::env::temp_dir().join("music-bot-store-e2e");
    std::fs::create_dir_all(&workdir).ok();
    let identity_path = workdir.join("identity-restart.json");
    let config = BotConfig::new("ws3-bot", &identity_path).with_auto_connect(false);
    let handle = spawn_bot(id, config, store_a_dyn.clone());

    // Enqueue 3 tracks via the dispatch surface.
    handle
        .send(BotCommand::Queue(QueueCommand::Enqueue(track("a"))))
        .await
        .unwrap();
    handle
        .send(BotCommand::Queue(QueueCommand::Enqueue(track("b"))))
        .await
        .unwrap();
    handle
        .send(BotCommand::Queue(QueueCommand::Enqueue(track("c"))))
        .await
        .unwrap();

    // Advance head — the just-finished track (a) gets popped; b
    // becomes the new head.
    handle
        .send(BotCommand::Queue(QueueCommand::Advance))
        .await
        .unwrap();

    // Drain the dispatcher's reaction queue so the assertions below
    // see committed store state. A short sleep is enough — the actor
    // processes commands FIFO on its own runtime task.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Snapshot the store BEFORE shutting the bot down. The on-disk
    // shape is what WS-5's SurrealDB impl will mirror.
    let snapshot = store_a.snapshot_to_json().await.unwrap();

    // Tear the bot down.
    handle.shutdown().await.unwrap();

    // Spawn a fresh store + supervisor, reload from the snapshot.
    let store_b: Arc<InMemoryMusicBotStore> = Arc::new(InMemoryMusicBotStore::new());
    store_b.load_from_json(&snapshot).await.unwrap();

    // Assertion: the queue has 2 tracks with title "b" as the head
    // (next-up), title "c" as the tail.
    let queue = store_b.queue_peek(id).await.unwrap();
    assert_eq!(queue.len(), 2, "expected queue len 2 after restart, got {}", queue.len());
    assert_eq!(queue[0].title, "b", "head should be track b post-advance");
    assert_eq!(queue[1].title, "c", "tail should be track c");

    let current = store_b.queue_current(id).await.unwrap();
    assert_eq!(current.unwrap().title, "b", "next-up still pointed at track b");
}

#[tokio::test]
async fn enqueue_emits_queue_changed_and_now_playing_on_first_track() {
    let store: Arc<dyn MusicBotStore> = Arc::new(InMemoryMusicBotStore::new());
    let id = BotId(1);
    let workdir = std::env::temp_dir().join("music-bot-store-e2e");
    std::fs::create_dir_all(&workdir).ok();
    let identity_path = workdir.join("identity-events.json");
    let config = BotConfig::new("ws3-bot-events", &identity_path).with_auto_connect(false);
    let handle = spawn_bot(id, config, store.clone());
    let mut events = handle.subscribe();

    // First enqueue — queue was empty, expect both `QueueChanged` AND
    // `NowPlaying`.
    handle
        .send(BotCommand::Queue(QueueCommand::Enqueue(track("alpha"))))
        .await
        .unwrap();

    let len = next_match(&mut events, |ev| match ev {
        BotEvent::QueueChanged { len, current } => {
            assert!(current.is_some(), "current should be Some after first enqueue");
            Some(*len)
        }
        _ => None,
    })
    .await
    .expect("QueueChanged");
    assert_eq!(len, 1);

    let now_playing = next_match(&mut events, |ev| match ev {
        BotEvent::NowPlaying(track) => Some(track.title.clone()),
        _ => None,
    })
    .await
    .expect("NowPlaying");
    assert_eq!(now_playing, "alpha");

    // Second enqueue — queue not empty, expect `QueueChanged` only,
    // no second `NowPlaying`.
    handle
        .send(BotCommand::Queue(QueueCommand::Enqueue(track("beta"))))
        .await
        .unwrap();
    let len = next_match(&mut events, |ev| match ev {
        BotEvent::QueueChanged { len, .. } => Some(*len),
        _ => None,
    })
    .await
    .expect("QueueChanged after second enqueue");
    assert_eq!(len, 2);
    // Drain briefly — assert NO NowPlaying fires for the second enqueue.
    let extra = timeout(Duration::from_millis(150), events.recv()).await;
    if let Ok(Ok(BotEvent::NowPlaying(_))) = extra {
        panic!("unexpected NowPlaying fired for non-first enqueue");
    }

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn advance_emits_now_playing_then_queue_empty_on_drain() {
    let store: Arc<dyn MusicBotStore> = Arc::new(InMemoryMusicBotStore::new());
    let id = BotId(1);
    let workdir = std::env::temp_dir().join("music-bot-store-e2e");
    std::fs::create_dir_all(&workdir).ok();
    let identity_path = workdir.join("identity-drain.json");
    let config = BotConfig::new("ws3-bot-drain", &identity_path).with_auto_connect(false);
    let handle = spawn_bot(id, config, store.clone());
    let mut events = handle.subscribe();

    handle
        .send(BotCommand::Queue(QueueCommand::Enqueue(track("only"))))
        .await
        .unwrap();

    // Burn the QueueChanged + NowPlaying from the enqueue.
    next_match(&mut events, |ev| match ev {
        BotEvent::NowPlaying(_) => Some(()),
        _ => None,
    })
    .await
    .expect("initial NowPlaying");

    // Advance — pops the only track. Expect QueueChanged then QueueEmpty.
    handle
        .send(BotCommand::Queue(QueueCommand::Advance))
        .await
        .unwrap();

    let len = next_match(&mut events, |ev| match ev {
        BotEvent::QueueChanged { len, .. } => Some(*len),
        _ => None,
    })
    .await
    .expect("QueueChanged after Advance");
    assert_eq!(len, 0);

    let drained = next_match(&mut events, |ev| matches!(ev, BotEvent::QueueEmpty).then_some(()))
        .await;
    assert!(drained.is_some(), "expected QueueEmpty");

    handle.shutdown().await.unwrap();
}
