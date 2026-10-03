//! One summon client's TeamSpeak connection.
//!
//! The client connects without asking for a channel, so it lands in the
//! server's default channel. TeamSpeak 6 refuses the whole connection
//! when the channel a client asks for at connect is full; a client that
//! asks for nothing cannot be refused that way. It subscribes to every
//! channel on this same connection (`Server::set_subscribed(true)`, which
//! tsclientlib sends as `channelsubscribeall`), finds the caller on its
//! own client list, and `clientmove`s there. It plays only once it is in
//! the caller's channel, and it disconnects when the song ends.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::sync::mpsc;
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
    Heard, ListedClient, LiveQuiet, Seat, SessionCmd, SlotLaunch, SummonDirector,
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

/// Why a summon client left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Stopped,
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
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Stopped => write!(f, "stopped"),
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
    subscribe_all(&mut con).context("subscribe channels")?;

    let Some(channel) = wait_for_caller(&mut con, caller).await? else {
        leave(&mut con).await;
        return Ok(Outcome::CallerNotFound);
    };
    let saved = saved_bot_uids(&director.saved_identities(&server)).await;
    let saved_here = uid_in_channel(&con, channel, &saved);
    let request = match director.seat(&server, summon, channel, saved_here) {
        Seat::Go { request } => request,
        Seat::Occupied => {
            leave(&mut con).await;
            return Ok(Outcome::Occupied);
        }
        Seat::SavedBot => {
            leave(&mut con).await;
            return Ok(Outcome::SavedBotChannel);
        }
        Seat::Gone => {
            leave(&mut con).await;
            return Ok(Outcome::Stopped);
        }
    };
    if own_channel(&con) != Some(channel) {
        let handle = move_to(&mut con, channel)?;
        if let Moved::Refused(err) = wait_moved(&mut con, handle, channel).await? {
            leave(&mut con).await;
            return Ok(Outcome::MoveRefused(err));
        }
    }
    info!(%server, summon, caller, channel, "summon client is in the caller's channel");
    if let Some((_, clients)) = snapshot(&con) {
        director.note_clients(&server, summon, &clients);
    }

    let mut current: Option<ActiveAudio> = None;
    let mut frames: Option<mpsc::Receiver<AudioMsg>> = None;
    let mut monitor = SendTimingMonitor::new();
    let volume = VolumeHandle::default();
    if let Err(err) = play(
        &live,
        &request,
        &mut con,
        &mut current,
        &mut frames,
        &volume,
    )
    .await
    {
        leave(&mut con).await;
        return Ok(Outcome::ResolveFailed(err));
    }

    let outcome = loop {
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break Outcome::Stopped;
                }
            }
            cmd = cmds.recv() => match cmd {
                Some(SessionCmd::Play(arg)) => {
                    if let Err(err) =
                        play(&live, &arg, &mut con, &mut current, &mut frames, &volume).await
                    {
                        break Outcome::ResolveFailed(err);
                    }
                }
                Some(SessionCmd::Stop) | None => break Outcome::Stopped,
            },
            ev = async { con.events().next().await } => match ev {
                Some(Ok(item)) => {
                    match react(&director, &server, summon, own, channel, &con, &item) {
                        React::Stay => {}
                        React::Play(arg) => {
                            if let Err(err) =
                                play(&live, &arg, &mut con, &mut current, &mut frames, &volume)
                                    .await
                            {
                                break Outcome::ResolveFailed(err);
                            }
                        }
                        React::Leave(outcome) => break outcome,
                    }
                }
                Some(Err(err)) => {
                    warn!(%server, summon, error = %err, "summon client stream error");
                    break Outcome::ConnectionLost;
                }
                None => break Outcome::ConnectionLost,
            },
            msg = recv_frame(&mut frames) => match msg {
                Some(AudioMsg::Frame { bytes, enqueued_at, .. }) => {
                    if let Err(err) =
                        audio::send_opus_frame(&mut con, &bytes, enqueued_at, &mut monitor, false)
                    {
                        break Outcome::SendFailed(err.to_string());
                    }
                }
                Some(AudioMsg::Finished) | None => break Outcome::SongEnded,
                Some(AudioMsg::CatchupDropped(_)) | Some(AudioMsg::PipelineEvent(_)) => {}
            },
        }
    };

    audio::tear_down(&mut current);
    audio::send_voice_stop(&mut con);
    leave(&mut con).await;
    Ok(outcome)
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

/// What one stream item means for a seated summon client.
#[derive(Debug, PartialEq, Eq)]
enum React {
    Stay,
    Play(String),
    Leave(Outcome),
}

fn react(
    director: &SummonDirector,
    server: &str,
    summon: u64,
    own: ClientId,
    channel: u64,
    con: &Connection,
    item: &StreamItem,
) -> React {
    match item {
        StreamItem::BookEvents(events) => {
            let Some((at, clients)) = snapshot(con) else {
                return React::Stay;
            };
            director.note_clients(server, summon, &clients);
            let mut next = React::Stay;
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
                    Line::Song(arg) => next = React::Play(arg),
                    Line::Stop => return React::Leave(Outcome::Stopped),
                    Line::Crossing => {
                        director.hear(server, invoker.id.0, message, Heard::Crossing);
                    }
                    Line::Ignore => {}
                }
            }
            if at != channel {
                return React::Leave(Outcome::MovedAway);
            }
            if !someone_else_in(&clients, channel, own.0) {
                return React::Leave(Outcome::ChannelEmpty);
            }
            next
        }
        StreamItem::DisconnectedTemporarily(_) => React::Leave(Outcome::ConnectionLost),
        _ => React::Stay,
    }
}

/// A chat line as a seated summon client sees it.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    /// `!play` or `!radio` typed in this client's channel.
    Song(String),
    /// `!stop` typed in this client's channel.
    Stop,
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

fn someone_else_in(clients: &[ListedClient], channel: u64, own: u16) -> bool {
    clients
        .iter()
        .any(|client| client.channel_id == channel && client.id != own)
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
        assert_eq!(line_for(MessageTarget::Channel, "!np"), Line::Ignore);
        assert_eq!(line_for(MessageTarget::Channel, "hello"), Line::Ignore);
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
        assert!(!someone_else_in(&clients, 42, 5));
        assert!(someone_else_in(&clients, 9, 5));
    }

    #[test]
    fn outcomes_say_why_the_client_left() {
        assert_eq!(
            Outcome::MoveRefused("ChannelMaxclientsReached".into()).to_string(),
            "the server refused the move: ChannelMaxclientsReached"
        );
        assert_eq!(Outcome::SongEnded.to_string(), "the song ended");
    }
}
