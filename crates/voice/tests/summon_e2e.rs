//! Summon end-to-end against the local TS6 fixture.
//!
//! Two people sit in two channels the saved bot is not in. One channel is
//! called "Tech Support" and has room for exactly one more client, like
//! the live channel the old quiet clients asked for at connect. Each
//! person asks the saved bot for a song from their own channel: one by
//! private message, one by poke. The summon cap is 1. Each person
//! must still get a temporary client of their own, in their channel, and
//! hear its audio there. One `!stop`s it from that channel's chat; the
//! other plays to the end. Both clients leave, and their identity files
//! go with them.
//!
//! While the summon client fills "Tech Support", a client that names that
//! channel at connect must be refused with `ChannelMaxclientsReached`.
//! That refusal is what killed every quiet client in the handshake.
//!
//! Every client here connects from one IP, so the test waits for the
//! fixture's antiflood points to decay before it summons. TeamSpeak
//! refuses a host with `ConnectFailedBanned` after about four connects in
//! a burst; that refusal is what the old quiet pool ran into on the live
//! server. Leave a minute between runs for the same reason.
//!
//! `summon_home_e2e` picks a home channel. A summon client that is done
//! waits there instead of leaving, a summon from another channel takes it
//! without a new connection, it comes back after the song, it follows a
//! new home, and it leaves when the home is cleared.
//!
//! Gated like the other fixture tests: feature + env + `#[ignore]`. The
//! two tests take turns, and the second waits for the antiflood points of
//! the first to decay.
//!
//!     make ts6-up
//!     TS6_VOICE_FIXTURE=1 cargo test -p music-bot --features lifecycle-e2e \
//!         --test summon_e2e -- --ignored --nocapture

#![cfg(feature = "lifecycle-e2e")]

extern crate music_bot as bot_lib;

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bot_lib::{BotConfig, BotEvent, BotSupervisor};
use futures::StreamExt;
use tokio::sync::{broadcast, oneshot};
use tokio::time::{Instant, sleep, timeout};
use tsclientlib::prelude::*;
use tsclientlib::{ClientId, Connection, DisconnectOptions, MessageTarget, Reason, StreamItem};
use tsproto_packets::packets::{AudioData, Direction, Flags, OutCommand, PacketType};

use ts6_voice_fixture::{load_or_create_identity, wait_for_connected};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const SUMMON_TIMEOUT: Duration = Duration::from_secs(45);
/// Long enough for the late client to connect while the summon client
/// still fills "Tech Support".
const TONE_MS: u64 = 20_000;
/// 20 s of 20 ms frames is 1000. Leave room for a slow fixture.
const MIN_FRAMES: usize = 200;
/// Every client in this test connects from one IP: three connects and
/// their commands so far, then two summons. The default antiflood block
/// (250 points, about 50 per connect, 5 decayed per second) needs this
/// pause to leave room for them.
const FLOOD_SETTLE: Duration = Duration::from_secs(30);
/// Same reason, before the sixth connect from this IP.
const LATE_SETTLE: Duration = Duration::from_secs(12);
/// Ada types `!stop` after 16 s of the 20 s tone, after the late client.
const STOP_AFTER_FRAMES: usize = 800;
/// Songs in the home test. Short: the test is about where the client goes
/// after them.
const SHORT_TONE_MS: u64 = 4_000;
/// How long a move or a song may take to show in a person's book.
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// One fixture test at a time: every connect comes from the same IP.
static FIXTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn summon_e2e() {
    if !env_flag("TS6_VOICE_FIXTURE") {
        eprintln!(
            "[summon_e2e] skipped — set TS6_VOICE_FIXTURE=1 after `make ts6-up`. \
             See docs/ts6-fixture.md."
        );
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tsclientlib=warn,tsproto=warn".into()),
        )
        .with_test_writer()
        .try_init();

    let _turn = FIXTURE.lock().await;
    if let Err(err) = run().await {
        eprintln!("\n=== summon_e2e failed ===");
        for (i, cause) in err.chain().enumerate() {
            eprintln!("  [{i}] {cause}");
        }
        panic!("summon_e2e failed: {err:#}");
    }
    // Let this test's connects decay before the next fixture test.
    sleep(FLOOD_SETTLE).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn summon_home_e2e() {
    if !env_flag("TS6_VOICE_FIXTURE") {
        eprintln!(
            "[summon_home_e2e] skipped. Set TS6_VOICE_FIXTURE=1 after `make ts6-up`. \
             See docs/ts6-fixture.md."
        );
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tsclientlib=warn,tsproto=warn".into()),
        )
        .with_test_writer()
        .try_init();

    let _turn = FIXTURE.lock().await;
    if let Err(err) = run_home().await {
        eprintln!("\n=== summon_home_e2e failed ===");
        for (i, cause) in err.chain().enumerate() {
            eprintln!("  [{i}] {cause}");
        }
        panic!("summon_home_e2e failed: {err:#}");
    }
    sleep(FLOOD_SETTLE).await;
}

async fn run() -> Result<()> {
    let addr = env::var("TS6_VOICE_FIXTURE_ADDR").unwrap_or_else(|_| "127.0.0.1:9987".into());
    let tag = std::process::id();
    let workdir = env::temp_dir().join(format!("music-bot-summon-e2e-{tag}"));
    let _ = std::fs::remove_dir_all(&workdir);
    let summon_dir = workdir.join("quiet-identities");
    let cookie: Arc<RwLock<Option<PathBuf>>> = Arc::new(RwLock::new(None));
    let key: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));

    // 1. One saved bot, summon on with cap 1.
    let supervisor = BotSupervisor::new();
    supervisor.enable_quiet_sessions(summon_dir.clone(), Arc::clone(&cookie), Arc::clone(&key));
    let saved_identity = workdir.join("bot-1.identity");
    let bot_id = supervisor
        .spawn(
            BotConfig::new("qa-summon-desk", &saved_identity)
                .with_server_addr(&addr)
                .with_handshake_timeout(HANDSHAKE_TIMEOUT)
                .with_auto_connect(true),
            Arc::clone(&cookie),
            Arc::clone(&key),
        )
        .await;
    supervisor.note_summon_push(&addr, bot_id.0, Some(1), &saved_identity);
    let mut bot_events = supervisor
        .subscribe(bot_id)
        .await
        .context("saved bot events")?;
    let bot_clid = wait_bot_connected(&mut bot_events).await?;
    eprintln!("saved bot connected as client {bot_clid}");

    // 2. Two people, each in a channel of their own.
    let tech_name = format!("Tech Support {tag}");
    let lounge_name = format!("Lounge {tag}");
    let mut ada = connect_person(&addr, &workdir, "qa-ada").await?;
    let tech = create_own_channel(&mut ada, &tech_name, Some(2)).await?;
    let mut bo = connect_person(&addr, &workdir, "qa-bo").await?;
    let lounge = create_own_channel(&mut bo, &lounge_name, None).await?;
    eprintln!("ada in {tech_name:?} ({tech}), bo in {lounge_name:?} ({lounge})");
    keep_polling(&mut [&mut ada, &mut bo], FLOOD_SETTLE).await;

    // 3. Ada asks by private message, Bo by poke. A default TeamSpeak 6
    //    server lets guests do both; server chat is for admins only.
    send(
        &mut ada,
        MessageTarget::Client(ClientId(bot_clid)),
        &format!("!play synthetic://?hz=440&duration_ms={TONE_MS}&amplitude=0.4"),
    )
    .await
    .context("ada's private message")?;
    send(
        &mut bo,
        MessageTarget::Poke(ClientId(bot_clid)),
        &format!("!play synthetic://?hz=660&duration_ms={TONE_MS}&amplitude=0.4"),
    )
    .await
    .context("bo's poke")?;

    let (tech_seated_tx, tech_seated) = oneshot::channel();
    // Ada stops hers from her own channel's chat; Bo's plays to the end.
    let ada_watch = tokio::spawn(watch_summon(
        ada,
        tech,
        bot_clid,
        Some(tech_seated_tx),
        Some(STOP_AFTER_FRAMES),
    ));
    let bo_watch = tokio::spawn(watch_summon(bo, lounge, bot_clid, None, None));

    // 4. While the summon client fills "Tech Support", naming that channel
    //    at connect is refused for the whole connection.
    timeout(SUMMON_TIMEOUT, tech_seated)
        .await
        .context("no summon client reached Tech Support")?
        .context("Tech Support watcher ended early")?;
    sleep(LATE_SETTLE).await;
    let refusal = refused_at_connect(&addr, &workdir, &tech_name).await?;
    eprintln!("connecting into the full channel: {refusal}");
    if !refusal.contains("ChannelMaxclientsReached") {
        bail!("expected ChannelMaxclientsReached, got {refusal}");
    }
    let both = supervisor.summon().quiet_count(&addr);
    eprintln!("summon clients while playing: {both}");

    // 5. Each person got their own client, heard it, and saw it leave.
    let ada_seen = ada_watch.await.context("ada watcher")??;
    let bo_seen = bo_watch.await.context("bo watcher")??;
    eprintln!("ada: {ada_seen:?}");
    eprintln!("bo: {bo_seen:?}");
    if ada_seen.summon == bo_seen.summon {
        bail!("both channels got the same client {}", ada_seen.summon);
    }
    for (who, seen) in [("ada", &ada_seen), ("bo", &bo_seen)] {
        if seen.frames < MIN_FRAMES {
            bail!(
                "{who} heard {} frames from the summon client, want at least {MIN_FRAMES}",
                seen.frames
            );
        }
        if !seen.left {
            bail!("{who}'s summon client did not leave after the song");
        }
    }
    let full_song = (TONE_MS / 20) as usize;
    if ada_seen.frames >= full_song - 50 {
        bail!(
            "`!stop` in Ada's channel did not end her summon early ({} frames)",
            ada_seen.frames
        );
    }
    if bo_seen.frames < full_song - 50 {
        bail!("Bo's song stopped early ({} frames)", bo_seen.frames);
    }
    if both < 2 {
        bail!(
            "the director held {both} summon clients while two channels played; cap 1 must not turn the second person away"
        );
    }

    // 6. The saved bot played neither line and is still the only saved bot.
    while let Ok(ev) = bot_events.try_recv() {
        if matches!(ev, BotEvent::NowPlaying(_) | BotEvent::QueueChanged { .. }) {
            bail!("the saved bot took a summon line as its own command: {ev:?}");
        }
    }
    if supervisor.list().await.len() != 1 {
        bail!("summon clients must not be saved bots");
    }
    wait_until(
        || supervisor.summon().quiet_count(&addr) == 0,
        "summon slots freed",
    )
    .await?;
    wait_until(
        || summon_identity_files(&summon_dir) == 0,
        "summon identity files removed",
    )
    .await?;

    supervisor.shutdown_bot(bot_id).await.ok();
    let _ = std::fs::remove_dir_all(&workdir);
    Ok(())
}

async fn run_home() -> Result<()> {
    let addr = env::var("TS6_VOICE_FIXTURE_ADDR").unwrap_or_else(|_| "127.0.0.1:9987".into());
    let tag = std::process::id();
    let workdir = env::temp_dir().join(format!("music-bot-summon-home-e2e-{tag}"));
    let _ = std::fs::remove_dir_all(&workdir);
    let summon_dir = workdir.join("quiet-identities");
    let cookie: Arc<RwLock<Option<PathBuf>>> = Arc::new(RwLock::new(None));
    let key: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));

    // 1. One saved bot, summon on with cap 1.
    let supervisor = BotSupervisor::new();
    supervisor.enable_quiet_sessions(summon_dir.clone(), Arc::clone(&cookie), Arc::clone(&key));
    let saved_identity = workdir.join("bot-1.identity");
    let bot_id = supervisor
        .spawn(
            BotConfig::new("qa-summon-home", &saved_identity)
                .with_server_addr(&addr)
                .with_handshake_timeout(HANDSHAKE_TIMEOUT)
                .with_auto_connect(true),
            Arc::clone(&cookie),
            Arc::clone(&key),
        )
        .await;
    supervisor.note_summon_push(&addr, bot_id.0, Some(1), &saved_identity);
    let mut bot_events = supervisor
        .subscribe(bot_id)
        .await
        .context("saved bot events")?;
    let bot_clid = wait_bot_connected(&mut bot_events).await?;

    // 2. Ada sits in the bot room, which becomes the home. Bo sits in a
    //    lounge.
    let room_name = format!("Bot room {tag}");
    let lounge_name = format!("Lounge {tag}");
    let mut ada = Person::new(connect_person(&addr, &workdir, "qa-home-ada").await?);
    let room = create_own_channel(&mut ada.con, &room_name, None).await?;
    let mut bo = Person::new(connect_person(&addr, &workdir, "qa-home-bo").await?);
    let lounge = create_own_channel(&mut bo.con, &lounge_name, None).await?;
    supervisor
        .set_summon_home(&addr, Some(room))
        .map_err(anyhow::Error::msg)?;
    eprintln!("home {room_name:?} ({room}), lounge {lounge_name:?} ({lounge})");
    let settle_until = Instant::now() + FLOOD_SETTLE;
    while Instant::now() < settle_until {
        poll_once(&mut [&mut ada, &mut bo]).await?;
    }

    // 3. Ada summons from inside the home. The client plays there and,
    //    when the song is done, stays there.
    send(
        &mut ada.con,
        MessageTarget::Client(ClientId(bot_clid)),
        &format!("!play synthetic://?hz=440&duration_ms={SHORT_TONE_MS}&amplitude=0.4"),
    )
    .await
    .context("ada's private message")?;
    let mut summon = 0u16;
    poll_until(
        &mut [&mut ada, &mut bo],
        "a summon client playing in the home",
        SUMMON_TIMEOUT,
        |people| {
            let Some(id) = people[0].summons_in(room).first().copied() else {
                return false;
            };
            summon = id;
            people[0].frames(id) >= MIN_FRAMES / 2
        },
    )
    .await?;
    let first_ids = supervisor.summon().summon_ids(&addr);
    poll_until(
        &mut [&mut ada, &mut bo],
        "the summon client waiting in the home after its song",
        STEP_TIMEOUT,
        |people| {
            supervisor.summon().waiting_ids(&addr).len() == 1
                && people[0].channel_of(summon) == Some(room)
        },
    )
    .await?;
    eprintln!("summon client {summon} waits in the home after Ada's song");

    // 4. Bo summons from the lounge. The waiting client comes over; no
    //    second client connects.
    send(
        &mut bo.con,
        MessageTarget::Poke(ClientId(bot_clid)),
        &format!("!play synthetic://?hz=660&duration_ms={SHORT_TONE_MS}&amplitude=0.4"),
    )
    .await
    .context("bo's poke")?;
    poll_until(
        &mut [&mut ada, &mut bo],
        "the waiting client playing in the lounge",
        SUMMON_TIMEOUT,
        |people| {
            people[1].channel_of(summon) == Some(lounge)
                && people[1].frames(summon) >= MIN_FRAMES / 2
        },
    )
    .await?;
    if supervisor.summon().summon_ids(&addr) != first_ids {
        bail!(
            "Bo's summon started another client: {:?} after {first_ids:?}",
            supervisor.summon().summon_ids(&addr)
        );
    }
    if bo.summons_seen() != 1 || ada.summons_seen() != 1 {
        bail!("a second summon client showed up on the server");
    }
    poll_until(
        &mut [&mut ada, &mut bo],
        "the summon client back in the home after Bo's song",
        STEP_TIMEOUT,
        |people| {
            supervisor.summon().waiting_ids(&addr).len() == 1
                && people[0].channel_of(summon) == Some(room)
        },
    )
    .await?;
    eprintln!("summon client {summon} served Bo and went home");

    // 5. A new home moves the waiting client.
    supervisor
        .set_summon_home(&addr, Some(lounge))
        .map_err(anyhow::Error::msg)?;
    poll_until(
        &mut [&mut ada, &mut bo],
        "the waiting client in the new home",
        STEP_TIMEOUT,
        |people| {
            supervisor.summon().waiting_ids(&addr).len() == 1
                && people[1].channel_of(summon) == Some(lounge)
        },
    )
    .await?;

    // 6. No home: the waiting client leaves the server.
    supervisor
        .set_summon_home(&addr, None)
        .map_err(anyhow::Error::msg)?;
    poll_until(
        &mut [&mut ada, &mut bo],
        "the waiting client gone once no home is picked",
        STEP_TIMEOUT,
        |people| {
            people[1].channel_of(summon).is_none() && supervisor.summon().quiet_count(&addr) == 0
        },
    )
    .await?;
    wait_until(
        || summon_identity_files(&summon_dir) == 0,
        "summon identity files removed",
    )
    .await?;

    while let Ok(ev) = bot_events.try_recv() {
        if matches!(ev, BotEvent::NowPlaying(_) | BotEvent::QueueChanged { .. }) {
            bail!("the saved bot took a summon line as its own command: {ev:?}");
        }
    }
    for person in [ada, bo] {
        person.leave().await;
    }
    supervisor.shutdown_bot(bot_id).await.ok();
    let _ = std::fs::remove_dir_all(&workdir);
    Ok(())
}

/// A person's connection and the audio frames it received, per sender.
struct Person {
    con: Connection,
    frames_from: HashMap<u16, usize>,
    summons: std::collections::BTreeSet<u16>,
}

impl Person {
    fn new(con: Connection) -> Self {
        Self {
            con,
            frames_from: HashMap::new(),
            summons: std::collections::BTreeSet::new(),
        }
    }

    /// Summon clients in `channel` by their client ids.
    fn summons_in(&self, channel: u64) -> Vec<u16> {
        let Ok(book) = self.con.get_state() else {
            return Vec::new();
        };
        book.clients
            .iter()
            .filter(|(_, client)| client.channel.0 == channel && client.name.starts_with("Summon"))
            .map(|(id, _)| id.0)
            .collect()
    }

    fn channel_of(&self, client: u16) -> Option<u64> {
        let book = self.con.get_state().ok()?;
        book.clients
            .get(&ClientId(client))
            .map(|client| client.channel.0)
    }

    fn frames(&self, from: u16) -> usize {
        self.frames_from.get(&from).copied().unwrap_or(0)
    }

    /// Summon clients this person has seen anywhere on the server.
    fn summons_seen(&self) -> usize {
        self.summons.len()
    }

    async fn leave(mut self) {
        let _ = self.con.disconnect(
            DisconnectOptions::new()
                .reason(Reason::Clientdisconnect)
                .message("test done".to_string()),
        );
        drain_for(&mut self.con, Duration::from_millis(300)).await;
    }
}

/// Poll every person until `done` holds.
async fn poll_until(
    people: &mut [&mut Person],
    what: &str,
    limit: Duration,
    mut done: impl FnMut(&[&Person]) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + limit;
    loop {
        {
            let view: Vec<&Person> = people.iter().map(|person| &**person).collect();
            if done(&view) {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for {what}");
        }
        poll_once(people).await?;
    }
}

/// One short poll of every person: count audio frames per sender and note
/// every summon client on the server.
async fn poll_once(people: &mut [&mut Person]) -> Result<()> {
    for person in people.iter_mut() {
        match timeout(Duration::from_millis(20), person.con.events().next()).await {
            Ok(Some(Ok(StreamItem::Audio(packet)))) => {
                if let AudioData::S2C { from, data, .. } = packet.data().data()
                    && !data.is_empty()
                {
                    *person.frames_from.entry(*from).or_default() += 1;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => bail!("person stream error: {err}"),
            Ok(None) => bail!("person stream ended"),
            Err(_) => {}
        }
        if let Ok(book) = person.con.get_state() {
            for (id, client) in book.clients.iter() {
                if client.name.starts_with("Summon") {
                    person.summons.insert(id.0);
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct Seen {
    summon: u16,
    frames: usize,
    left: bool,
}

/// Poll `con` until a summon client sits in `channel`, count its audio,
/// and wait for it to leave.
/// With `stop_after`, type `!stop` in this channel's chat once that many
/// frames arrived from the summon client.
async fn watch_summon(
    mut con: Connection,
    channel: u64,
    bot_clid: u16,
    mut seated: Option<oneshot::Sender<u16>>,
    mut stop_after: Option<usize>,
) -> Result<Seen> {
    let own = con.get_state().context("person book")?.own_client.0;
    let deadline = Instant::now() + SUMMON_TIMEOUT;
    let mut summon: Option<u16> = None;
    let mut frames_from: HashMap<u16, usize> = HashMap::new();
    let mut left = false;
    loop {
        if let (Some(id), Some(after)) = (summon, stop_after)
            && frames_from.get(&id).copied().unwrap_or(0) >= after
        {
            stop_after = None;
            let cmd = {
                let book = con.get_state().context("person book")?;
                book.send_message(MessageTarget::Channel, "!stop")
            };
            cmd.send(&mut con).context("channel !stop")?;
        }
        if let Ok(book) = con.get_state() {
            let in_channel: Vec<u16> = book
                .clients
                .iter()
                .filter(|(id, client)| {
                    client.channel.0 == channel && id.0 != own && id.0 != bot_clid
                })
                .map(|(id, _)| id.0)
                .collect();
            match summon {
                None => {
                    if let Some(first) = in_channel.first() {
                        summon = Some(*first);
                        if let Some(tx) = seated.take() {
                            let _ = tx.send(*first);
                        }
                    }
                }
                Some(id) => {
                    if !in_channel.contains(&id) {
                        left = true;
                        break;
                    }
                }
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, con.events().next()).await {
            Ok(Some(Ok(StreamItem::Audio(packet)))) => {
                if let AudioData::S2C { from, data, .. } = packet.data().data()
                    && !data.is_empty()
                {
                    *frames_from.entry(*from).or_default() += 1;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => bail!("person stream error: {err}"),
            Ok(None) => bail!("person stream ended"),
            Err(_) => break,
        }
    }
    let summon = summon.context("no summon client came to this channel")?;
    let frames = frames_from.get(&summon).copied().unwrap_or(0);
    let _ = con.disconnect(
        DisconnectOptions::new()
            .reason(Reason::Clientdisconnect)
            .message("test done".to_string()),
    );
    drain_for(&mut con, Duration::from_millis(300)).await;
    Ok(Seen {
        summon,
        frames,
        left,
    })
}

async fn wait_bot_connected(events: &mut broadcast::Receiver<BotEvent>) -> Result<u16> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, events.recv()).await {
            Ok(Ok(BotEvent::Connected { client_id, .. })) => return Ok(client_id),
            Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => bail!("saved bot events closed"),
            Err(_) => bail!("saved bot did not connect within {HANDSHAKE_TIMEOUT:?}"),
        }
    }
}

async fn connect_person(addr: &str, workdir: &Path, name: &str) -> Result<Connection> {
    let identity = load_or_create_identity(&workdir.join(format!("{name}.identity")))
        .await
        .context("person identity")?;
    let mut con = Connection::build(addr)
        .name(name.to_string())
        .identity(identity)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
        .connect()
        .context("person connect")?;
    if !wait_for_connected(&mut con, HANDSHAKE_TIMEOUT)
        .await
        .context("person handshake")?
    {
        bail!("{name} did not connect within {HANDSHAKE_TIMEOUT:?}");
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while con
        .get_state()
        .ok()
        .and_then(|book| book.clients.get(&book.own_client))
        .is_none()
    {
        if Instant::now() > deadline {
            bail!("{name} never appeared in its own book");
        }
        let _ = timeout(Duration::from_millis(100), con.events().next()).await;
    }
    Ok(con)
}

/// Create a temporary channel. The server moves its creator in. A cap is
/// sent with `channel_flag_maxclients_unlimited=0`; without that flag the
/// server keeps a new channel unlimited.
async fn create_own_channel(con: &mut Connection, name: &str, max: Option<u16>) -> Result<u64> {
    let mut cmd = OutCommand::new(
        Direction::C2S,
        Flags::empty(),
        PacketType::Command,
        "channelcreate",
    );
    cmd.write_arg("channel_name", &name);
    if let Some(max) = max {
        cmd.write_arg("channel_maxclients", &max);
        cmd.write_arg("channel_flag_maxclients_unlimited", &0);
    }
    let handle = cmd.send_with_result(con).context("channelcreate")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(book) = con.get_state()
            && let Some(own) = book.clients.get(&book.own_client)
            && book
                .channels
                .get(&own.channel)
                .is_some_and(|channel| channel.name == name)
        {
            return Ok(own.channel.0);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("the server did not move the creator into {name:?}");
        }
        match timeout(remaining, con.events().next()).await {
            Ok(Some(Ok(StreamItem::MessageResult(got, Err(err))))) if got == handle => {
                bail!("channelcreate {name:?} refused: {err}");
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => bail!("stream error: {err}"),
            Ok(None) => bail!("stream ended"),
            Err(_) => {}
        }
    }
}

/// Send a text message or a poke and wait for the server to accept it.
async fn send(con: &mut Connection, target: MessageTarget, text: &str) -> Result<()> {
    let cmd = {
        let book = con.get_state().context("book")?;
        book.send_message(target, text)
    };
    let handle = cmd.send_with_result(con).context("send text message")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("the server did not answer the message");
        }
        match timeout(remaining, con.events().next()).await {
            Ok(Some(Ok(StreamItem::MessageResult(got, result)))) if got == handle => {
                return result.map_err(|err| anyhow::anyhow!("the server refused it: {err}"));
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(err))) => bail!("stream error: {err}"),
            Ok(None) => bail!("stream ended"),
            Err(_) => {}
        }
    }
}

/// Connect a client that names `channel` at connect, the way the old quiet
/// clients named "Tech Support". Returns the handshake error.
async fn refused_at_connect(addr: &str, workdir: &Path, channel: &str) -> Result<String> {
    let identity = load_or_create_identity(&workdir.join("qa-late.identity"))
        .await
        .context("late identity")?;
    let mut con = Connection::build(addr)
        .name("qa-late")
        .identity(identity)
        .channel(channel.to_string())
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
        .connect()
        .context("late connect")?;
    match wait_for_connected(&mut con, HANDSHAKE_TIMEOUT).await {
        Err(err) => Ok(format!("{err:#}")),
        Ok(true) => {
            let _ = con.disconnect(DisconnectOptions::new());
            bail!("the server let a client name a full channel at connect")
        }
        Ok(false) => bail!("the late handshake neither finished nor failed"),
    }
}

fn summon_identity_files(dir: &Path) -> usize {
    let Ok(servers) = std::fs::read_dir(dir) else {
        return 0;
    };
    servers
        .flatten()
        .filter_map(|server| std::fs::read_dir(server.path()).ok())
        .flat_map(|files| files.flatten())
        .filter(|file| file.file_name().to_string_lossy().starts_with("summon-"))
        .count()
}

async fn wait_until(mut done: impl FnMut() -> bool, what: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}");
        }
        sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Keep several connections answering pings while the test waits.
async fn keep_polling(cons: &mut [&mut Connection], dur: Duration) {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        for con in cons.iter_mut() {
            let _ = timeout(Duration::from_millis(50), con.events().next()).await;
        }
    }
}

async fn drain_for(con: &mut Connection, dur: Duration) {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining, con.events().next()).await {
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }
}

fn env_flag(name: &str) -> bool {
    matches!(env::var(name).as_deref(), Ok("1") | Ok("true") | Ok("yes"))
}
