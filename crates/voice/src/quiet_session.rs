//! One summon client's TeamSpeak connection.
//!
//! The client connects without asking for a channel, so it lands in the
//! server's default channel. TeamSpeak 6 refuses the whole connection
//! when the channel a client asks for at connect is full; a client that
//! asks for nothing cannot be refused that way. It subscribes to every
//! channel on this same connection (`Server::set_subscribed(true)`, which
//! tsclientlib sends as `channelsubscribeall`), finds the caller on its
//! own client list, and `clientmove`s there. It plays only once it is in
//! the caller's channel.
//!
//! When it is not playing for anyone it goes to the home channel picked
//! on the music page and waits there on the same connection. The next
//! summon takes it from there. With no home picked it disconnects
//! instead, so it never waits in a public channel.
//!
//! `!pause` holds the send loop: frames stop and the voice connection
//! stays up. The client stays in the channel. `!resume` continues that
//! same playback. `!stop` still ends the song and sends the client home.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use tsclientlib::prelude::*;
use tsclientlib::{
    ChannelId as TsChannelId, ClientId, ConnectOptions, Connection, DisconnectOptions, Identity,
    MessageHandle, MessageTarget, Reason, StreamItem, TsError, events::Event as BookEvent,
};

use music_bot_audio::VolumeHandle;

use crate::audio::{self, ActiveAudio, AudioMsg, SendTimingMonitor};
use crate::chat::{ParsedCommand, parse as parse_chat};
use crate::summon::{
    Heard, Homeward, ListedClient, LiveQuiet, Seat, SessionCmd, SlotLaunch, SummonDirector,
    cold_audio_source, discard_quiet_identity, is_summon_line, mint_quiet_identity,
};

/// Nickname of a summon client. The server renames a duplicate.
const SUMMON_NAME: &str = "Summon";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const OWN_CLIENT_WAIT: Duration = Duration::from_secs(2);
/// How long the client list may take to show the caller after
/// `channelsubscribeall`.
const CALLER_WAIT: Duration = Duration::from_secs(3);
const MOVE_WAIT: Duration = Duration::from_secs(5);
const DISCONNECT_DRAIN: Duration = Duration::from_secs(1);

/// Why a summon client stopped playing for someone, or why it left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Stopped,
    StopCommand,
    CallerNotFound,
    Occupied,
    SavedBotChannel,
    MoveRefused(String),
    ResolveFailed(String),
    SongEnded,
    ChannelEmpty,
    MovedAway,
    ConnectionLost,
    SendFailed(String),
    NoHome,
    OverLimit,
    HomeRefused(String),
}

impl Outcome {
    /// The client disconnects instead of going home.
    fn ends_session(&self) -> bool {
        matches!(
            self,
            Outcome::Stopped | Outcome::ConnectionLost | Outcome::SendFailed(_)
        )
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Stopped => write!(f, "stopped"),
            Outcome::StopCommand => write!(f, "!stop in its channel"),
            Outcome::CallerNotFound => write!(f, "the caller was not on its client list"),
            Outcome::Occupied => write!(f, "another summon client plays in that channel"),
            Outcome::SavedBotChannel => write!(f, "a saved bot sits in that channel"),
            Outcome::MoveRefused(err) => write!(f, "the server refused the move: {err}"),
            Outcome::ResolveFailed(err) => write!(f, "the song did not start: {err}"),
            Outcome::SongEnded => write!(f, "the song ended"),
            Outcome::ChannelEmpty => write!(f, "nobody is left in the channel"),
            Outcome::MovedAway => write!(f, "it was moved out of the channel"),
            Outcome::ConnectionLost => write!(f, "the connection dropped"),
            Outcome::SendFailed(err) => write!(f, "send_audio failed: {err}"),
            Outcome::NoHome => write!(f, "no home channel is picked"),
            Outcome::OverLimit => write!(f, "the server is over its limit of summon clients"),
            Outcome::HomeRefused(err) => {
                write!(f, "the server refused the move to the home channel: {err}")
            }
        }
    }
}

pub(crate) async fn run(
    director: SummonDirector,
    launch: SlotLaunch,
    live: LiveQuiet,
) -> Result<Outcome> {
    let SlotLaunch {
        server,
        dial,
        summon,
        caller,
        identity_path,
        connect_at,
        mut stop,
        mut cmds,
    } = launch;
    let _end = SessionEnd {
        director: director.clone(),
        server: server.clone(),
        summon,
        identity_path: Some(identity_path.clone()),
    };
    // Paced so a burst of summons cannot trip the server's flood
    // protection for every client on this host.
    let slot = tokio::time::sleep_until(connect_at.into());
    tokio::pin!(slot);
    loop {
        if *stop.borrow() {
            return Ok(Outcome::Stopped);
        }
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() {
                    return Ok(Outcome::Stopped);
                }
            }
            _ = &mut slot => break,
        }
    }
    let identity = mint_quiet_identity(&identity_path)
        .await
        .context("mint summon identity")?;
    let mut con = connect_options(&dial, identity)
        .connect()
        .context("summon client connect")?;

    if let Err(err) = handshake(&mut con).await {
        if is_flood_refusal(&err) {
            director.note_flood_refusal(&server);
        }
        return Err(err.context("summon client handshake"));
    }
    let own = wait_own(&mut con).await?;
    director.note_own_client(&server, summon, own.0);
    subscribe_all(&mut con).context("subscribe channels")?;

    let mut session = Session {
        director: &director,
        server: &server,
        summon,
        own,
        live: &live,
        con: &mut con,
        stop: &mut stop,
        cmds: &mut cmds,
    };
    let outcome = session.run(caller).await;
    leave(&mut con).await;
    outcome
}

/// A connected summon client.
struct Session<'a> {
    director: &'a SummonDirector,
    server: &'a str,
    summon: u64,
    own: ClientId,
    live: &'a LiveQuiet,
    con: &'a mut Connection,
    stop: &'a mut watch::Receiver<bool>,
    cmds: &'a mut mpsc::UnboundedReceiver<SessionCmd>,
}

/// What ended a wait in the home channel.
enum Waited {
    /// The director picked this client for a summon.
    Serve(u16),
    /// The home channel changed.
    Rehome,
    Leave(Outcome),
}

/// Where a trip to the home channel ended.
enum Trip {
    Arrived(u64),
    Leave(Outcome),
}

impl Session<'_> {
    /// Serve `caller`, then wait at home for the next summon, until a
    /// reason to disconnect comes up. Returns that reason.
    async fn run(&mut self, caller: u16) -> Result<Outcome> {
        let (server, summon) = (self.server, self.summon);
        let mut next = Some(caller);
        loop {
            if let Some(caller) = next.take() {
                let outcome = self.serve(caller).await?;
                if outcome.ends_session() {
                    return Ok(outcome);
                }
                info!(%server, summon, caller, %outcome, "summon request ended");
            }
            let home = match self.go_home().await? {
                Trip::Arrived(home) => home,
                Trip::Leave(outcome) => return Ok(outcome),
            };
            info!(%server, summon, home, "summon client is waiting in the home channel");
            match self.wait_at_home(home).await {
                Waited::Serve(caller) => next = Some(caller),
                Waited::Rehome => {}
                Waited::Leave(outcome) => return Ok(outcome),
            }
        }
    }

    /// Find `caller`, go to their channel, and play until the song ends
    /// or the channel stops it. Says why it stopped playing.
    async fn serve(&mut self, caller: u16) -> Result<Outcome> {
        let (director, server, summon, own) = (self.director, self.server, self.summon, self.own);
        let (live, con, stop, cmds) = (self.live, &mut *self.con, &mut *self.stop, &mut *self.cmds);
        let Some(channel) = wait_for_caller(con, caller).await? else {
            return Ok(Outcome::CallerNotFound);
        };
        let saved = saved_bot_uids(&director.saved_identities(server)).await;
        let saved_here = uid_in_channel(con, channel, &saved);
        let request = match director.seat(server, summon, channel, saved_here) {
            Seat::Go { request } => request,
            Seat::Occupied => return Ok(Outcome::Occupied),
            Seat::SavedBot => return Ok(Outcome::SavedBotChannel),
            Seat::Gone => return Ok(Outcome::Stopped),
        };
        if own_channel(con) != Some(channel) {
            let handle = move_to(con, channel)?;
            if let Moved::Refused(err) = wait_moved(con, handle, channel).await? {
                return Ok(Outcome::MoveRefused(err));
            }
        }
        info!(%server, summon, caller, channel, "summon client is in the caller's channel");
        if let Some((_, clients)) = snapshot(con) {
            director.note_clients(server, summon, &clients);
        }

        let mut current: Option<ActiveAudio> = None;
        let mut frames: Option<mpsc::Receiver<AudioMsg>> = None;
        let mut monitor = SendTimingMonitor::new();
        let volume = VolumeHandle::default();
        // `!pause` closes this gate and parks the sibling. Frames stop
        // and the voice connection stays up.
        let mut hold = PlaybackHold::new();
        if let Err(err) = play(live, &request, con, &mut current, &mut frames, &volume).await {
            return Ok(Outcome::ResolveFailed(err));
        }

        let outcome = 'play: loop {
            tokio::select! {
                biased;
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break Outcome::Stopped;
                    }
                }
                cmd = cmds.recv() => match cmd {
                    Some(SessionCmd::Play(arg)) => {
                        hold.clear_for_new_song();
                        if let Err(err) =
                            play(live, &arg, con, &mut current, &mut frames, &volume).await
                        {
                            break Outcome::ResolveFailed(err);
                        }
                    }
                    // Sent only to a client that waits at home.
                    Some(SessionCmd::Serve(_) | SessionCmd::Home) => {}
                    Some(SessionCmd::Stop) | None => break Outcome::Stopped,
                },
                ev = async { con.events().next().await } => match ev {
                    Some(Ok(item)) => {
                        for step in react(director, server, summon, own, channel, con, &item) {
                            match step {
                                React::Stay => {}
                                React::Pause => hold.apply(&current, true),
                                React::Resume => hold.apply(&current, false),
                                React::Play(arg) => {
                                    hold.clear_for_new_song();
                                    if let Err(err) =
                                        play(live, &arg, con, &mut current, &mut frames, &volume)
                                            .await
                                    {
                                        break 'play Outcome::ResolveFailed(err);
                                    }
                                }
                                React::Done(outcome) => break 'play outcome,
                            }
                        }
                    }
                    Some(Err(err)) => {
                        warn!(%server, summon, error = %err, "summon client stream error");
                        break Outcome::ConnectionLost;
                    }
                    None => break Outcome::ConnectionLost,
                },
                msg = recv_frame_unless(&mut frames, hold.paused) => match msg {
                    Some(AudioMsg::Frame { bytes, enqueued_at, .. }) => {
                        let enqueued_at = hold.stamp(enqueued_at);
                        if let Err(err) =
                            send_quiet_frame(con, &bytes, enqueued_at, &mut monitor)
                        {
                            break Outcome::SendFailed(err);
                        }
                    }
                    Some(AudioMsg::Finished) | None => break Outcome::SongEnded,
                    Some(AudioMsg::CatchupDropped(_)) | Some(AudioMsg::PipelineEvent(_)) => {}
                },
            }
        };

        audio::tear_down(&mut current);
        audio::send_voice_stop(con);
        Ok(outcome)
    }

    /// Go to the home channel. A client with nowhere to wait leaves.
    async fn go_home(&mut self) -> Result<Trip> {
        let (director, server, summon) = (self.director, self.server, self.summon);
        let con = &mut *self.con;
        let mut verdict = director.release(server, summon);
        loop {
            let home = match verdict {
                Homeward::Home(home) => home,
                Homeward::NoHome => return Ok(Trip::Leave(Outcome::NoHome)),
                Homeward::OverLimit => return Ok(Trip::Leave(Outcome::OverLimit)),
                Homeward::Gone => return Ok(Trip::Leave(Outcome::Stopped)),
            };
            if own_channel(con) != Some(home) {
                let handle = move_to(con, home)?;
                if let Moved::Refused(err) = wait_moved(con, handle, home).await? {
                    return Ok(Trip::Leave(Outcome::HomeRefused(err)));
                }
            }
            verdict = director.settle(server, summon, home);
            if verdict == Homeward::Home(home) {
                return Ok(Trip::Arrived(home));
            }
        }
    }

    /// Wait in `home` until a summon picks this client, the home
    /// changes, or a reason to disconnect comes up.
    async fn wait_at_home(&mut self, home: u64) -> Waited {
        let (director, server, summon, own) = (self.director, self.server, self.summon, self.own);
        let (con, stop, cmds) = (&mut *self.con, &mut *self.stop, &mut *self.cmds);
        loop {
            tokio::select! {
                biased;
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return Waited::Leave(Outcome::Stopped);
                    }
                }
                cmd = cmds.recv() => match cmd {
                    Some(SessionCmd::Serve(caller)) => return Waited::Serve(caller),
                    Some(SessionCmd::Home) => return Waited::Rehome,
                    // A song for the channel it played in before.
                    Some(SessionCmd::Play(_)) => {}
                    Some(SessionCmd::Stop) | None => return Waited::Leave(Outcome::Stopped),
                },
                ev = async { con.events().next().await } => match ev {
                    Some(Ok(item)) => {
                        if let Some(outcome) =
                            react_at_home(director, server, summon, own, home, con, &item)
                        {
                            return Waited::Leave(outcome);
                        }
                    }
                    Some(Err(err)) => {
                        warn!(%server, summon, error = %err, "summon client stream error");
                        return Waited::Leave(Outcome::ConnectionLost);
                    }
                    None => return Waited::Leave(Outcome::ConnectionLost),
                },
            }
        }
    }
}

/// Drive the event stream until the server sends the first book. A
/// refusal keeps the server's error code: the log prints it, and a flood
/// refusal starts the cool-down.
async fn handshake(con: &mut Connection) -> Result<()> {
    let deadline = tokio::time::sleep(HANDSHAKE_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            _ = &mut deadline => {
                anyhow::bail!("handshake did not finish within {HANDSHAKE_TIMEOUT:?}")
            }
            ev = async { con.events().next().await } => match ev {
                Some(Ok(StreamItem::BookEvents(_))) => return Ok(()),
                Some(Ok(_)) => continue,
                Some(Err(err)) => return Err(anyhow::Error::new(err)),
                None => anyhow::bail!("stream ended before the handshake finished"),
            }
        }
    }
}

/// True when the server refused the connection as banned or flooding.
/// TeamSpeak answers a host that connected too often with
/// `ConnectFailedBanned` until its antiflood points decay.
fn is_flood_refusal(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<tsclientlib::Error>(),
        Some(tsclientlib::Error::ConnectTs(
            TsError::ConnectFailedBanned | TsError::BanFlooding | TsError::ClientIsFlooding
        ))
    )
}

/// Connect options for a summon client. There is no `.channel(..)`: a
/// full channel named at connect makes TeamSpeak 6 refuse the handshake
/// with `ChannelMaxclientsReached`. The move happens after connect.
fn connect_options(dial: &str, identity: Identity) -> ConnectOptions {
    Connection::build(dial)
        .name(SUMMON_NAME)
        .identity(identity)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
}

/// Start `request` as the song. A failed resolve is the summon's end.
async fn play(
    live: &LiveQuiet,
    request: &str,
    con: &mut Connection,
    current: &mut Option<ActiveAudio>,
    frames: &mut Option<mpsc::Receiver<AudioMsg>>,
    volume: &VolumeHandle,
) -> std::result::Result<(), String> {
    let cookie = live
        .yt_cookie
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    *frames = None;
    if audio::tear_down(current) {
        audio::send_voice_stop(con);
    }
    let source = cold_audio_source(request);
    match audio::start_pipeline(current, &source, cookie, volume).await {
        Ok(_) => {
            *frames = current.as_mut().and_then(|active| active.audio_rx.take());
            Ok(())
        }
        Err(err) => {
            *frames = None;
            audio::tear_down(current);
            Err(err.to_string())
        }
    }
}

async fn recv_frame(frames: &mut Option<mpsc::Receiver<AudioMsg>>) -> Option<AudioMsg> {
    match frames {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The send loop's frame arm. While `paused` is set this stays pending,
/// so a frame already queued is left there and nothing goes on the wire.
async fn recv_frame_unless(
    frames: &mut Option<mpsc::Receiver<AudioMsg>>,
    paused: bool,
) -> Option<AudioMsg> {
    if paused {
        std::future::pending().await
    } else {
        recv_frame(frames).await
    }
}

/// Whether the seated send loop is holding frames, and how long the
/// last hold lasted. The sibling parks on the same transitions. The
/// pipeline stays spawned, so resume continues the same playback.
struct PlaybackHold {
    paused: bool,
    started: Option<Instant>,
    /// When the last hold lifted, and how long it lasted. A frame stamped
    /// before that lift waited through the hold, which is not a send stall.
    lifted: Option<(Instant, Duration)>,
}

impl PlaybackHold {
    fn new() -> Self {
        Self {
            paused: false,
            started: None,
            lifted: None,
        }
    }

    fn apply(&mut self, current: &Option<ActiveAudio>, pause: bool) {
        if pause {
            if !self.paused {
                self.started = Some(Instant::now());
            }
            self.paused = true;
        } else if self.paused {
            if let Some(started) = self.started.take() {
                self.lifted = Some((Instant::now(), started.elapsed()));
            }
            self.paused = false;
        }
        if let Some(active) = current {
            active.set_paused(self.paused);
        }
    }

    /// A new song replaces the pipeline. Its frames were not held.
    fn clear_for_new_song(&mut self) {
        self.paused = false;
        self.started = None;
        self.lifted = None;
    }

    /// Move a stamp that waited through the hold forward by the hold, so
    /// the send path's dequeue gap is the wait outside the pause.
    fn stamp(&self, enqueued_at: Instant) -> Instant {
        match self.lifted {
            Some((lifted, held)) if enqueued_at < lifted => {
                enqueued_at.checked_add(held).unwrap_or(enqueued_at)
            }
            _ => enqueued_at,
        }
    }
}

fn send_quiet_frame(
    con: &mut impl audio::OutgoingVoice,
    bytes: &[u8],
    enqueued_at: std::time::Instant,
    monitor: &mut SendTimingMonitor,
) -> Result<(), String> {
    audio::send_opus_frame(con, bytes, enqueued_at, monitor, false).map_err(|err| err.to_string())
}

/// What one stream item means for a seated summon client.
#[derive(Debug, PartialEq, Eq)]
enum React {
    Stay,
    Play(String),
    /// Hold the send loop. The connection and the channel stay.
    Pause,
    /// Continue the playback the pause held.
    Resume,
    /// It stops playing for the caller, for this reason.
    Done(Outcome),
}

#[cfg(test)]
impl React {
    /// Pause and resume leave the client where it is. A `Done` ends the
    /// song, and the session then sends the client home or away.
    fn leaves_the_channel(&self) -> bool {
        matches!(self, React::Done(_))
    }
}

fn react(
    director: &SummonDirector,
    server: &str,
    summon: u64,
    own: ClientId,
    channel: u64,
    con: &Connection,
    item: &StreamItem,
) -> Vec<React> {
    match item {
        StreamItem::BookEvents(events) => {
            let Some((at, clients)) = snapshot(con) else {
                return Vec::new();
            };
            director.note_clients(server, summon, &clients);
            // Commands in this batch are kept in order. A later pause
            // must not replace an earlier song.
            let mut batch = Vec::new();
            for event in events {
                let BookEvent::Message {
                    target,
                    invoker,
                    message,
                } = event
                else {
                    continue;
                };
                if invoker.id == own {
                    continue;
                }
                match line_for(*target, message) {
                    Line::Crossing => {
                        director.hear(server, invoker.id.0, message, Heard::Crossing);
                    }
                    line => {
                        if let Some(outcome) = note_command(&mut batch, line) {
                            return vec![React::Done(outcome)];
                        }
                    }
                }
            }
            if at != channel {
                return vec![React::Done(Outcome::MovedAway)];
            }
            // Other summon clients waiting in this channel are not
            // listeners.
            let mut ours = director.summon_clients(server);
            ours.push(own.0);
            if !someone_else_in(&clients, channel, &ours) {
                return vec![React::Done(Outcome::ChannelEmpty)];
            }
            batch
        }
        StreamItem::DisconnectedTemporarily(_) => vec![React::Done(Outcome::ConnectionLost)],
        _ => Vec::new(),
    }
}

/// What one stream item means for a client waiting in the home channel.
/// `Some` is why it leaves.
fn react_at_home(
    director: &SummonDirector,
    server: &str,
    summon: u64,
    own: ClientId,
    home: u64,
    con: &Connection,
    item: &StreamItem,
) -> Option<Outcome> {
    match item {
        StreamItem::BookEvents(events) => {
            let (at, clients) = snapshot(con)?;
            director.note_clients(server, summon, &clients);
            for event in events {
                let BookEvent::Message {
                    target,
                    invoker,
                    message,
                } = event
                else {
                    continue;
                };
                if invoker.id == own || !is_summon_line(message) {
                    continue;
                }
                match target {
                    // Someone in the home channel. A saved bot or a summon
                    // client playing there answers that chat itself.
                    MessageTarget::Channel => {
                        director.hear_home_chat(server, invoker.id.0, message);
                    }
                    MessageTarget::Server | MessageTarget::Client(_) | MessageTarget::Poke(_) => {
                        director.hear(server, invoker.id.0, message, Heard::Crossing);
                    }
                }
            }
            // Moved or kicked out of the home channel by someone else.
            (at != home).then_some(Outcome::MovedAway)
        }
        StreamItem::DisconnectedTemporarily(_) => Some(Outcome::ConnectionLost),
        _ => None,
    }
}

/// A chat line as a seated summon client sees it.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    /// `!play` or `!radio` typed in this client's channel.
    Song(String),
    /// `!stop` typed in this client's channel.
    Stop,
    /// `!pause` typed in this client's channel.
    Pause,
    /// `!resume` or `!unpause` typed in this client's channel.
    Resume,
    /// A summon line sent as server chat, a private message, or a poke.
    /// The director decides where it goes.
    Crossing,
    Ignore,
}

fn line_for(target: MessageTarget, message: &str) -> Line {
    match target {
        // Channel chat arrives only from the channel this client is in.
        MessageTarget::Channel => match parse_chat(message) {
            Ok(ParsedCommand::Play { arg } | ParsedCommand::Radio { arg }) => Line::Song(arg),
            Ok(ParsedCommand::Stop) => Line::Stop,
            Ok(ParsedCommand::Pause) => Line::Pause,
            Ok(ParsedCommand::Resume) => Line::Resume,
            _ => Line::Ignore,
        },
        MessageTarget::Server | MessageTarget::Client(_) | MessageTarget::Poke(_) => {
            if is_summon_line(message) {
                Line::Crossing
            } else {
                Line::Ignore
            }
        }
    }
}

/// What a channel command means for a client that is already seated.
/// Pause and resume are not songs: they do not start a resolve and they
/// do not leave the channel.
fn command_react(line: Line) -> React {
    match line {
        Line::Song(arg) => React::Play(arg),
        Line::Stop => React::Done(Outcome::StopCommand),
        Line::Pause => React::Pause,
        Line::Resume => React::Resume,
        Line::Crossing | Line::Ignore => React::Stay,
    }
}

/// Append one channel command. `Some` is a stop, which ends the batch
/// the way a single command always did. A pause is appended, so it does
/// not replace a song already in the batch.
fn note_command(batch: &mut Vec<React>, line: Line) -> Option<Outcome> {
    match command_react(line) {
        React::Done(outcome) => Some(outcome),
        React::Stay => None,
        react => {
            batch.push(react);
            None
        }
    }
}

/// True when someone other than `ours`, this server's summon clients,
/// is in `channel`.
fn someone_else_in(clients: &[ListedClient], channel: u64, ours: &[u16]) -> bool {
    clients
        .iter()
        .any(|client| client.channel_id == channel && !ours.contains(&client.id))
}

fn subscribe_all(con: &mut Connection) -> Result<()> {
    let cmd = {
        let book = con.get_state().context("connection has no book yet")?;
        book.server.set_subscribed(true)
    };
    cmd.send(con)
        .context("channelsubscribeall on the summon client's connection")?;
    Ok(())
}

fn move_to(con: &mut Connection, channel: u64) -> Result<MessageHandle> {
    let cmd = {
        let book = con.get_state().context("connection has no book yet")?;
        let own = book
            .clients
            .get(&book.own_client)
            .context("own client missing")?;
        own.client_move(TsChannelId(channel))
    };
    cmd.send_with_result(con).context("clientmove")
}

enum Moved {
    Arrived,
    Refused(String),
}

/// Wait until the book shows this client in `channel`, or the server
/// refuses the move. A full or locked channel is a refusal, not an error.
async fn wait_moved(con: &mut Connection, handle: MessageHandle, channel: u64) -> Result<Moved> {
    let deadline = tokio::time::sleep(MOVE_WAIT);
    tokio::pin!(deadline);
    loop {
        if own_channel(con) == Some(channel) {
            return Ok(Moved::Arrived);
        }
        tokio::select! {
            biased;
            _ = &mut deadline => anyhow::bail!("clientmove to channel {channel} was not confirmed"),
            ev = async { con.events().next().await } => match ev {
                Some(Ok(item)) => {
                    if let Some(refused) = move_refusal(handle, &item) {
                        return Ok(Moved::Refused(refused));
                    }
                }
                Some(Err(err)) => return Err(anyhow::anyhow!("stream error: {err}")),
                None => anyhow::bail!("stream ended before the move finished"),
            }
        }
    }
}

/// The server's error text when `item` is the refusal of `handle`.
fn move_refusal(handle: MessageHandle, item: &StreamItem) -> Option<String> {
    match item {
        StreamItem::MessageResult(got, Err(err)) if *got == handle => Some(err.to_string()),
        _ => None,
    }
}

fn snapshot(con: &Connection) -> Option<(u64, Vec<ListedClient>)> {
    let book = con.get_state().ok()?;
    let own = book.clients.get(&book.own_client)?;
    let at = own.channel.0;
    let clients = book
        .clients
        .iter()
        .map(|(id, client)| ListedClient {
            id: id.0,
            channel_id: client.channel.0,
        })
        .collect();
    Some((at, clients))
}

fn own_channel(con: &Connection) -> Option<u64> {
    let book = con.get_state().ok()?;
    book.clients.get(&book.own_client).map(|own| own.channel.0)
}

fn caller_channel(con: &Connection, caller: u16) -> Option<u64> {
    let book = con.get_state().ok()?;
    book.clients
        .get(&ClientId(caller))
        .map(|client| client.channel.0)
}

/// Wait for the caller to show up on the list `channelsubscribeall`
/// fills. `None` when the caller is not on the server, or sits in a
/// channel this client may not subscribe to.
async fn wait_for_caller(con: &mut Connection, caller: u16) -> Result<Option<u64>> {
    let deadline = tokio::time::sleep(CALLER_WAIT);
    tokio::pin!(deadline);
    loop {
        if let Some(channel) = caller_channel(con, caller) {
            return Ok(Some(channel));
        }
        tokio::select! {
            biased;
            _ = &mut deadline => return Ok(None),
            ev = async { con.events().next().await } => match ev {
                Some(Ok(_)) => continue,
                Some(Err(err)) => return Err(anyhow::anyhow!("stream error: {err}")),
                None => anyhow::bail!("stream ended before the caller showed up"),
            }
        }
    }
}

async fn wait_own(con: &mut Connection) -> Result<ClientId> {
    let deadline = tokio::time::sleep(OWN_CLIENT_WAIT);
    tokio::pin!(deadline);
    loop {
        if let Some(id) = own_id(con) {
            return Ok(id);
        }
        tokio::select! {
            biased;
            _ = &mut deadline => anyhow::bail!("summon client never appeared in its own book"),
            ev = async { con.events().next().await } => match ev {
                Some(Ok(_)) => continue,
                Some(Err(err)) => return Err(anyhow::anyhow!("stream error: {err}")),
                None => anyhow::bail!("stream ended before the summon client appeared"),
            }
        }
    }
}

fn own_id(con: &Connection) -> Option<ClientId> {
    let book = con.get_state().ok()?;
    book.clients.get(&book.own_client)?;
    Some(book.own_client)
}

/// UIDs of the saved bots' identity files. A file that is missing or
/// does not parse is skipped.
async fn saved_bot_uids(paths: &[PathBuf]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for path in paths {
        let Ok(raw) = tokio::fs::read_to_string(path).await else {
            continue;
        };
        let Ok(identity) = serde_json::from_str::<Identity>(raw.trim()) else {
            continue;
        };
        out.push(identity.key().to_pub().get_uid_no_base64());
    }
    out
}

fn uid_in_channel(con: &Connection, channel: u64, uids: &[Vec<u8>]) -> bool {
    let Ok(book) = con.get_state() else {
        return false;
    };
    book.clients.values().any(|client| {
        client.channel.0 == channel
            && client
                .uid
                .as_ref()
                .is_some_and(|uid| uids.iter().any(|saved| saved == &uid.0))
    })
}

/// Disconnect and give the server a moment to see it.
async fn leave(con: &mut Connection) {
    let _ = con.disconnect(
        DisconnectOptions::new()
            .reason(Reason::Clientdisconnect)
            .message("summon ended".to_string()),
    );
    let deadline = tokio::time::sleep(DISCONNECT_DRAIN);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            _ = &mut deadline => break,
            ev = async { con.events().next().await } => match ev {
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => break,
            }
        }
    }
}

/// Frees the summon's place and removes its identity file however the
/// session ends.
struct SessionEnd {
    director: SummonDirector,
    server: String,
    summon: u64,
    identity_path: Option<PathBuf>,
}

impl Drop for SessionEnd {
    fn drop(&mut self) {
        self.director.session_ended(&self.server, self.summon);
        if let Some(path) = self.identity_path.take()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move { discard_quiet_identity(&path).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsclientlib::CommandError;

    #[test]
    fn summon_clients_do_not_ask_for_a_channel_at_connect() {
        let options = connect_options("voice.example:9987", Identity::create());
        let debug = format!("{options:?}");
        assert!(debug.contains("channel: None"), "{debug}");
        assert!(!debug.contains("Tech Support"), "{debug}");
    }

    #[test]
    fn a_banned_or_flooding_refusal_starts_the_cool_down() {
        for code in [
            TsError::ConnectFailedBanned,
            TsError::BanFlooding,
            TsError::ClientIsFlooding,
        ] {
            let err = anyhow::Error::new(tsclientlib::Error::ConnectTs(code))
                .context("summon client handshake");
            assert!(is_flood_refusal(&err), "{code:?}");
        }
        let full = anyhow::Error::new(tsclientlib::Error::ConnectTs(
            TsError::ChannelMaxclientsReached,
        ));
        assert!(!is_flood_refusal(&full));
        assert!(!is_flood_refusal(&anyhow::anyhow!("timed out")));
    }

    #[test]
    fn a_refused_clientmove_is_the_pending_handle() {
        let handle = MessageHandle(7);
        let refused = StreamItem::MessageResult(
            handle,
            Err(CommandError {
                error: TsError::ChannelMaxclientsReached,
                missing_permission: None,
            }),
        );
        assert!(move_refusal(handle, &refused).is_some());
        assert_eq!(move_refusal(MessageHandle(8), &refused), None);
        let accepted = StreamItem::MessageResult(handle, Ok(()));
        assert_eq!(move_refusal(handle, &accepted), None);
    }

    #[test]
    fn channel_chat_is_this_clients_command() {
        assert_eq!(
            line_for(MessageTarget::Channel, "!play yt:next"),
            Line::Song("yt:next".into())
        );
        assert_eq!(
            line_for(MessageTarget::Channel, "!radio https://radio.example/live"),
            Line::Song("https://radio.example/live".into())
        );
        assert_eq!(line_for(MessageTarget::Channel, "!stop"), Line::Stop);
        assert_eq!(line_for(MessageTarget::Channel, "!pause"), Line::Pause);
        assert_eq!(line_for(MessageTarget::Channel, "!resume"), Line::Resume);
        assert_eq!(line_for(MessageTarget::Channel, "!unpause"), Line::Resume);
        assert_eq!(line_for(MessageTarget::Channel, "!np"), Line::Ignore);
        assert_eq!(line_for(MessageTarget::Channel, "hello"), Line::Ignore);
    }

    #[test]
    fn pause_and_resume_do_not_end_playback_or_start_a_song() {
        for line in ["!pause", "!resume", "!unpause"] {
            let react = command_react(line_for(MessageTarget::Channel, line));
            assert!(
                matches!(react, React::Pause | React::Resume),
                "{line} -> {react:?}"
            );
            // Not Done: the client stays connected, stays in the channel,
            // and the summon slot stays taken. Not a song: nothing new
            // starts, including in the home channel.
            assert!(!react.leaves_the_channel(), "{line}");
            assert!(!matches!(react, React::Play(_)), "{line}");
        }
    }

    #[test]
    fn a_pause_in_the_same_batch_does_not_swallow_a_song() {
        let mut song_then_pause = Vec::new();
        assert!(
            note_command(
                &mut song_then_pause,
                line_for(MessageTarget::Channel, "!play yt:next")
            )
            .is_none()
        );
        assert!(
            note_command(
                &mut song_then_pause,
                line_for(MessageTarget::Channel, "!pause")
            )
            .is_none()
        );
        assert!(matches!(
            song_then_pause.as_slice(),
            [React::Play(_), React::Pause]
        ));

        let mut pause_then_song = Vec::new();
        assert!(
            note_command(
                &mut pause_then_song,
                line_for(MessageTarget::Channel, "!pause")
            )
            .is_none()
        );
        assert!(
            note_command(
                &mut pause_then_song,
                line_for(MessageTarget::Channel, "!play yt:after")
            )
            .is_none()
        );
        assert!(matches!(
            pause_then_song.as_slice(),
            [React::Pause, React::Play(_)]
        ));
    }

    #[test]
    fn stop_still_sends_the_client_home() {
        assert_eq!(line_for(MessageTarget::Channel, "!stop"), Line::Stop);
        assert!(matches!(
            command_react(Line::Stop),
            React::Done(Outcome::StopCommand)
        ));
        // Stop ends the song. It does not drop the session, so the client
        // goes to the home channel instead of disconnecting.
        assert!(!Outcome::StopCommand.ends_session());
        assert!(Outcome::ConnectionLost.ends_session());
    }

    #[test]
    fn server_private_and_poke_lines_go_to_the_director() {
        for target in [
            MessageTarget::Server,
            MessageTarget::Client(ClientId(4)),
            MessageTarget::Poke(ClientId(4)),
        ] {
            assert_eq!(line_for(target, "!play yt:song"), Line::Crossing);
            assert_eq!(line_for(target, "!stop"), Line::Ignore);
            assert_eq!(line_for(target, "!pause"), Line::Ignore);
            assert_eq!(line_for(target, "!resume"), Line::Ignore);
            assert_eq!(line_for(target, "hi"), Line::Ignore);
        }
    }

    #[test]
    fn a_channel_with_only_this_client_is_empty() {
        let clients = [
            ListedClient {
                id: 5,
                channel_id: 42,
            },
            ListedClient {
                id: 6,
                channel_id: 9,
            },
        ];
        assert!(!someone_else_in(&clients, 42, &[5]));
        assert!(someone_else_in(&clients, 9, &[5]));
    }

    #[test]
    fn summon_clients_waiting_in_the_channel_are_not_listeners() {
        // The caller left the home channel. Two other summon clients
        // still wait there; the one that played is done.
        let clients = [
            ListedClient {
                id: 5,
                channel_id: 42,
            },
            ListedClient {
                id: 7,
                channel_id: 42,
            },
            ListedClient {
                id: 8,
                channel_id: 42,
            },
        ];
        assert!(!someone_else_in(&clients, 42, &[5, 7, 8]));
        assert!(someone_else_in(&clients, 42, &[5, 7]));
    }

    #[test]
    fn outcomes_say_why_the_client_left() {
        assert_eq!(
            Outcome::MoveRefused("ChannelMaxclientsReached".into()).to_string(),
            "the server refused the move: ChannelMaxclientsReached"
        );
        assert_eq!(Outcome::SongEnded.to_string(), "the song ended");
    }

    struct CountingVoice {
        sends: usize,
    }

    impl audio::OutgoingVoice for CountingVoice {
        fn send_audio(
            &mut self,
            _packet: tsproto_packets::packets::OutPacket,
        ) -> std::result::Result<(), tsclientlib::Error> {
            self.sends += 1;
            Ok(())
        }

        fn try_flush_outgoing(&mut self) -> std::result::Result<usize, tsclientlib::Error> {
            Ok(0)
        }
    }

    /// Pause parks the sibling and the send loop. Frames stop, the voice
    /// connection stays up, and resume reads the same pipeline.
    #[tokio::test]
    async fn pause_stops_frames_and_resume_continues_the_same_playback() {
        let mut current: Option<ActiveAudio> = None;
        let volume = VolumeHandle::default();
        let source = crate::command::AudioSource::Url(
            "synthetic://?hz=440&duration_ms=3000&amplitude=0.2".into(),
        );
        audio::start_pipeline(&mut current, &source, None, &volume)
            .await
            .expect("synthetic playback");
        let label = current.as_ref().expect("pipeline").source_label.clone();
        let mut frames = current.as_mut().and_then(|active| active.audio_rx.take());
        let mut voice = CountingVoice { sends: 0 };
        let mut monitor = SendTimingMonitor::new();
        let mut hold = PlaybackHold::new();

        send_until_paced(&mut frames, &mut voice, &mut monitor).await;
        let sends_before = voice.sends;

        hold.apply(&current, true);
        let held = tokio::time::timeout(
            Duration::from_millis(350),
            recv_frame_unless(&mut frames, hold.paused),
        )
        .await;
        assert!(held.is_err(), "the paused send loop took a frame");
        assert_eq!(voice.sends, sends_before, "a frame went out while paused");
        // Frames already queued stay off the wire. The sibling parks, so
        // the queue does not keep growing and the pipeline stays put.
        let _queued = drain_ready(&mut frames);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            drain_ready(&mut frames),
            0,
            "frames kept arriving while paused"
        );
        assert!(current.is_some(), "pause dropped the pipeline");
        assert_eq!(current.as_ref().unwrap().source_label, label);
        assert_eq!(voice.sends, sends_before, "pause wrote a voice-stop");

        hold.apply(&current, false);
        let resumed = tokio::time::timeout(
            Duration::from_secs(1),
            recv_frame_unless(&mut frames, hold.paused),
        )
        .await
        .expect("resume did not continue the playback");
        match resumed {
            Some(AudioMsg::Frame {
                bytes, enqueued_at, ..
            }) => {
                send_quiet_frame(&mut voice, &bytes, enqueued_at, &mut monitor)
                    .expect("voice connection still accepts frames");
            }
            other => panic!("resume did not continue the same playback: {other:?}"),
        }
        assert!(voice.sends > sends_before);
        assert!(current.is_some(), "resume replaced the pipeline");
        assert_eq!(current.as_ref().unwrap().source_label, label);
        assert!(frames.is_some(), "resume opened a second playback");
    }

    async fn send_until_paced(
        frames: &mut Option<mpsc::Receiver<AudioMsg>>,
        voice: &mut CountingVoice,
        monitor: &mut SendTimingMonitor,
    ) {
        let mut sent = 0usize;
        let mut last = std::time::Instant::now();
        loop {
            match recv_frame_unless(frames, false).await {
                Some(AudioMsg::Frame {
                    bytes, enqueued_at, ..
                }) => {
                    send_quiet_frame(voice, &bytes, enqueued_at, monitor)
                        .expect("voice connection still accepts frames");
                    sent += 1;
                    let now = std::time::Instant::now();
                    let gap = now.duration_since(last);
                    last = now;
                    if sent >= 3 && gap >= Duration::from_millis(15) {
                        return;
                    }
                }
                Some(AudioMsg::Finished) | None => {
                    panic!("playback ended before the send loop was pacing")
                }
                Some(AudioMsg::CatchupDropped(_) | AudioMsg::PipelineEvent(_)) => {}
            }
        }
    }

    #[test]
    fn a_pause_hold_is_not_a_send_stall() {
        // The send path warns when a frame's dequeue gap reaches 10 ms.
        // A hold is not that gap: the stamp moves forward by the hold.
        let enqueued = Instant::now();
        let held = Duration::from_millis(400);
        let lifted = enqueued + held;
        let hold = PlaybackHold {
            paused: false,
            started: None,
            lifted: Some((lifted, held)),
        };
        let stamp = hold.stamp(enqueued);
        let gap = lifted.saturating_duration_since(stamp);
        assert!(
            gap < Duration::from_millis(10),
            "the hold still looks like a loop stall: {gap:?}"
        );
        let after = lifted + Duration::from_millis(1);
        assert_eq!(hold.stamp(after), after);
    }

    fn drain_ready(frames: &mut Option<mpsc::Receiver<AudioMsg>>) -> usize {
        let Some(rx) = frames.as_mut() else {
            return 0;
        };
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }
}
