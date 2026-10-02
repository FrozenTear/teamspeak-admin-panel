//! One quiet client's TeamSpeak connection.
//!
//! The client connects in [`TECH_SUPPORT_CHANNEL`], then subscribes to
//! every channel on this same connection (`Server::set_subscribed(true)`,
//! which tsclientlib sends as `channelsubscribeall`). Moves are
//! `clientmove` on that connection. Opus frames are handed to
//! `send_audio` only while the director says the client is in the
//! caller's channel with a song open.

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::warn;
use tsclientlib::prelude::*;
use tsclientlib::{
    ChannelId as TsChannelId, ClientId, Connection, DisconnectOptions, MessageTarget, Reason,
    StreamItem, events::Event as BookEvent,
};

use music_bot_audio::VolumeHandle;

use crate::audio::{self, ActiveAudio, AudioMsg, SendTimingMonitor};
use crate::summon::{
    DeliveryHow, ListedClient, LiveQuiet, QuietInstruction, SlotLaunch, SummonArrival,
    SummonDirector, TECH_SUPPORT_CHANNEL, cold_audio_source, mint_quiet_identity,
};

pub(crate) async fn run(
    director: SummonDirector,
    launch: SlotLaunch,
    live: LiveQuiet,
) -> Result<()> {
    let server = launch.server.clone();
    let slot = launch.slot;
    let generation = launch.generation;
    let mut stop = launch.stop;
    let _guard = SessionEnd {
        director: director.clone(),
        server: server.clone(),
        slot,
        generation,
    };
    if *stop.borrow() {
        return Ok(());
    }
    let identity = mint_quiet_identity(&launch.identity_path)
        .await
        .context("mint quiet identity")?;
    let mut con = Connection::build(server.as_str())
        .name("Summon")
        .identity(identity)
        .channel(TECH_SUPPORT_CHANNEL)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
        .connect()
        .context("quiet client connect")?;

    let connected = ts6_voice_fixture::wait_for_connected(&mut con, Duration::from_secs(30))
        .await
        .context("quiet client handshake")?;
    if !connected {
        anyhow::bail!("quiet client handshake did not finish");
    }
    let mut own = wait_own(&mut con).await?;
    if !landed_in_tech_support(&mut con).await? {
        anyhow::bail!("quiet client did not land in {TECH_SUPPORT_CHANNEL}");
    }
    subscribe_all(&mut con).context("subscribe channels")?;
    if let Some((at, clients)) = snapshot(&con)
        && !director.mark_ready_gen(&server, slot, generation, at, own.0, &clients)
    {
        anyhow::bail!("quiet slot was replaced before it became ready");
    }

    let mut current: Option<ActiveAudio> = None;
    let mut frames: Option<mpsc::Receiver<AudioMsg>> = None;
    let mut monitor = SendTimingMonitor::new();
    let volume = VolumeHandle::default();
    let mut playback = false;
    let mut resubscribe = false;
    let mut resubscribe_tries: u8 = 0;
    let mut trust_on_next_book = false;

    loop {
        if *stop.borrow() {
            break;
        }
        if let Some(active) = current.as_ref() {
            active.set_paused(!playback);
        }
        tokio::select! {
            biased;
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
            ev = async { con.events().next().await } => {
                match ev {
                    Some(Ok(StreamItem::DisconnectedTemporarily(reason))) => {
                        warn!(%server, slot, ?reason, "quiet client reconnecting");
                        resubscribe = true;
                        resubscribe_tries = 0;
                    }
                    Some(Ok(_item)) if resubscribe => {
                        resubscribe_tries = resubscribe_tries.saturating_add(1);
                        if resubscribe_tries > 5 {
                            anyhow::bail!("quiet client did not resubscribe after reconnect");
                        }
                        if let Some(id) = own_id(&con) {
                            own = id;
                            if subscribe_all(&mut con).is_ok() {
                                if let Some((at, _)) = snapshot(&con) {
                                    director.mark_reconnected(&server, slot, generation, at);
                                }
                                resubscribe = false;
                                trust_on_next_book = true;
                            }
                        }
                    }
                    Some(Ok(item)) => {
                        if trust_on_next_book && matches!(item, StreamItem::BookEvents(_)) {
                            director.note_list_trusted(&server, slot, generation);
                            trust_on_next_book = false;
                        }
                        let instr = instructions_for(
                            &director,
                            &server,
                            slot,
                            generation,
                            own,
                            &con,
                            &item,
                        );
                        apply(
                            &director,
                            &live,
                            &server,
                            slot,
                            generation,
                            &mut con,
                            &mut current,
                            &mut frames,
                            &volume,
                            instr,
                        )
                        .await;
                        playback = director.playback_open_gen(&server, slot, generation);
                    }
                    Some(Err(err)) => {
                        warn!(%server, slot, error = %err, "quiet client stream error");
                        break;
                    }
                    None => break,
                }
            }
            msg = recv_frame(&mut frames) => {
                match msg {
                    Some(AudioMsg::Frame { bytes, enqueued_at, .. }) => {
                        if director.offer_frame_gen(&server, slot, generation)
                            && let Err(err) = audio::send_opus_frame(
                                &mut con,
                                &bytes,
                                enqueued_at,
                                &mut monitor,
                                false,
                            )
                        {
                            warn!(%server, slot, error = %err, "quiet client send_audio failed");
                            let instr =
                                director.on_playback_finished_gen(&server, slot, generation);
                            apply(
                                &director,
                                &live,
                                &server,
                                slot,
                                generation,
                                &mut con,
                                &mut current,
                                &mut frames,
                                &volume,
                                instr,
                            )
                            .await;
                            playback = director.playback_open_gen(&server, slot, generation);
                        }
                    }
                    Some(AudioMsg::Finished) | None => {
                        frames = None;
                        let instr = director.on_playback_finished_gen(&server, slot, generation);
                        apply(
                            &director,
                            &live,
                            &server,
                            slot,
                            generation,
                            &mut con,
                            &mut current,
                            &mut frames,
                            &volume,
                            instr,
                        )
                        .await;
                        playback = director.playback_open_gen(&server, slot, generation);
                    }
                    Some(AudioMsg::CatchupDropped(_)) | Some(AudioMsg::PipelineEvent(_)) => {}
                }
            }
        }
    }

    audio::tear_down(&mut current);
    audio::send_voice_stop(&mut con);
    let _ = con.disconnect(
        DisconnectOptions::new()
            .reason(Reason::Clientdisconnect)
            .message("summon ended".to_string()),
    );
    Ok(())
}

async fn recv_frame(frames: &mut Option<mpsc::Receiver<AudioMsg>>) -> Option<AudioMsg> {
    match frames {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn instructions_for(
    director: &SummonDirector,
    server: &str,
    slot: u32,
    generation: u64,
    own: ClientId,
    con: &Connection,
    item: &StreamItem,
) -> Vec<QuietInstruction> {
    // Voice packets are not book events. Snapshotting the client list
    // here would take the director lock on every incoming frame.
    let StreamItem::BookEvents(events) = item else {
        return Vec::new();
    };
    let Some((at, clients)) = snapshot(con) else {
        return Vec::new();
    };
    let mut out = director.on_book_gen(server, slot, generation, at, &clients);
    for event in events {
        let BookEvent::Message {
            target,
            invoker,
            message,
        } = event
        else {
            continue;
        };
        match target {
            // Channel text is delivered only inside the sender's channel.
            // It is a command when this connection is already there. It
            // is not how a summon typed somewhere else reaches Tech Support.
            MessageTarget::Channel => {
                if invoker.id == own {
                    continue;
                }
                let sender_here = clients
                    .iter()
                    .any(|client| client.id == invoker.id.0 && client.channel_id == at);
                if sender_here {
                    out.extend(director.on_chat_gen(
                        server,
                        slot,
                        generation,
                        invoker.id.0,
                        message,
                        &clients,
                    ));
                }
            }
            // Server text, a private message, or a poke can arrive here
            // while this client sits in Tech Support. The move happens
            // when that text is the staged summon, not before.
            MessageTarget::Server | MessageTarget::Client(_) | MessageTarget::Poke(_) => {
                if let Some(moved) =
                    director.ack_delivery_gen(server, slot, generation, message, &clients)
                {
                    out.extend(moved);
                } else if director.holds_staged_text(server, message) || invoker.id == own {
                    // The saved bot's copy of a summon, or our own echo.
                } else {
                    out.extend(director.on_chat_gen(
                        server,
                        slot,
                        generation,
                        invoker.id.0,
                        message,
                        &clients,
                    ));
                }
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn apply(
    director: &SummonDirector,
    live: &LiveQuiet,
    server: &str,
    slot: u32,
    generation: u64,
    con: &mut Connection,
    current: &mut Option<ActiveAudio>,
    frames: &mut Option<mpsc::Receiver<AudioMsg>>,
    volume: &VolumeHandle,
    instrs: Vec<QuietInstruction>,
) {
    let mut queue: VecDeque<QuietInstruction> = instrs.into();
    while let Some(instr) = queue.pop_front() {
        // A reply from the channel this client is sitting in would not
        // reach the caller. The director leaves `reply` empty; do not
        // invent one.
        let _ = instr.reply;
        if instr.stop_audio || instr.shutdown {
            *frames = None;
            if audio::tear_down(current) {
                audio::send_voice_stop(con);
            }
        }
        if let Some(channel) = instr.move_to {
            if let Some(active) = current.as_ref() {
                active.set_paused(true);
            }
            if let Err(err) = move_to(con, channel) {
                warn!(%server, slot, error = %err, "quiet client move failed");
                for follow in director
                    .on_move_failed_gen(server, slot, generation)
                    .into_iter()
                    .rev()
                {
                    queue.push_front(follow);
                }
            }
        }
        if let Some(arg) = instr.play
            && director.playback_open_gen(server, slot, generation)
        {
            let cookie = live
                .yt_cookie
                .read()
                .unwrap_or_else(|err| err.into_inner())
                .clone();
            let source = cold_audio_source(&arg);
            *frames = None;
            if audio::tear_down(current) {
                audio::send_voice_stop(con);
            }
            match audio::start_pipeline(current, &source, cookie, volume).await {
                Ok(_) => {
                    *frames = current.as_mut().and_then(|active| active.audio_rx.take());
                }
                Err(err) => {
                    warn!(%server, slot, error = %err, "quiet client cold resolve failed");
                    *frames = None;
                    audio::tear_down(current);
                    let back = director.on_playback_finished_gen(server, slot, generation);
                    for back in back {
                        if back.stop_audio {
                            audio::send_voice_stop(con);
                        }
                        if let Some(channel) = back.move_to {
                            let _ = move_to(con, channel);
                        }
                    }
                }
            }
        }
        if instr.shutdown {
            *frames = None;
            audio::tear_down(current);
        }
    }
}

/// Server text, a private message, or a poke. Never channel text.
fn crossing_target(arrival: &SummonArrival) -> MessageTarget {
    match arrival.how {
        DeliveryHow::Server => MessageTarget::Server,
        DeliveryHow::Private => MessageTarget::Client(ClientId(arrival.target)),
        DeliveryHow::Poke => MessageTarget::Poke(ClientId(arrival.target)),
    }
}

pub(crate) fn send_crossing(con: &mut Connection, arrival: &SummonArrival) -> Result<()> {
    let target = crossing_target(arrival);
    debug_assert!(
        !matches!(target, MessageTarget::Channel),
        "a crossing summon is not channel text"
    );
    let cmd = {
        let book = con.get_state().context("connection has no book yet")?;
        book.send_message(target, &arrival.text)
    };
    cmd.send(con).context("crossing summon")?;
    Ok(())
}

fn subscribe_all(con: &mut Connection) -> Result<()> {
    let cmd = {
        let book = con.get_state().context("connection has no book yet")?;
        book.server.set_subscribed(true)
    };
    cmd.send(con)
        .context("channelsubscribeall on the quiet client's connection")?;
    Ok(())
}

fn move_to(con: &mut Connection, channel: u64) -> Result<()> {
    let cmd = {
        let book = con.get_state().context("connection has no book yet")?;
        let own = book
            .clients
            .get(&book.own_client)
            .context("own client missing")?;
        own.client_move(TsChannelId(channel))
    };
    cmd.send(con).context("clientmove")?;
    Ok(())
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

async fn wait_own(con: &mut Connection) -> Result<ClientId> {
    let deadline = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(deadline);
    loop {
        if let Some(id) = own_id(con) {
            return Ok(id);
        }
        tokio::select! {
            biased;
            _ = &mut deadline => anyhow::bail!("quiet client never appeared in its own book"),
            ev = async { con.events().next().await } => match ev {
                Some(Ok(_)) => continue,
                Some(Err(err)) => return Err(anyhow::anyhow!("stream error: {err}")),
                None => anyhow::bail!("stream ended before the quiet client appeared"),
            }
        }
    }
}

fn own_id(con: &Connection) -> Option<ClientId> {
    let book = con.get_state().ok()?;
    book.clients.get(&book.own_client)?;
    Some(book.own_client)
}

struct SessionEnd {
    director: SummonDirector,
    server: String,
    slot: u32,
    generation: u64,
}

impl Drop for SessionEnd {
    fn drop(&mut self) {
        self.director
            .session_ended(&self.server, self.slot, self.generation);
    }
}

/// True only when this connection is in the channel named Tech Support.
/// A missing or rejected channel leaves the client in the default
/// channel; that is not a place to sit or to play.
async fn landed_in_tech_support(con: &mut Connection) -> Result<bool> {
    let deadline = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(deadline);
    loop {
        if let Some(name) = own_channel_name(con) {
            return Ok(name == TECH_SUPPORT_CHANNEL);
        }
        tokio::select! {
            biased;
            _ = &mut deadline => return Ok(false),
            ev = async { con.events().next().await } => match ev {
                Some(Ok(_)) => continue,
                Some(Err(err)) => return Err(anyhow::anyhow!("stream error: {err}")),
                None => anyhow::bail!("stream ended before the channel name arrived"),
            }
        }
    }
}

fn own_channel_name(con: &Connection) -> Option<String> {
    let book = con.get_state().ok()?;
    let own = book.clients.get(&book.own_client)?;
    let channel = book.channels.get(&own.channel)?;
    Some(channel.name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossing_delivery_is_not_channel_text() {
        let text = "!play https://cdn.example/one.mp3".to_string();
        for how in [DeliveryHow::Server, DeliveryHow::Private, DeliveryHow::Poke] {
            let target = crossing_target(&SummonArrival {
                how,
                target: 1,
                text: text.clone(),
            });
            assert!(!matches!(target, MessageTarget::Channel));
        }
        assert!(matches!(
            crossing_target(&SummonArrival {
                how: DeliveryHow::Private,
                target: 1,
                text: text.clone(),
            }),
            MessageTarget::Client(ClientId(1))
        ));
        assert!(matches!(
            crossing_target(&SummonArrival {
                how: DeliveryHow::Poke,
                target: 1,
                text: text.clone(),
            }),
            MessageTarget::Poke(ClientId(1))
        ));
        assert!(matches!(
            crossing_target(&SummonArrival {
                how: DeliveryHow::Server,
                target: 0,
                text,
            }),
            MessageTarget::Server
        ));
    }

    #[test]
    fn voice_packets_are_not_book_work() {
        let item = StreamItem::IdentityLevelIncreased;
        assert!(!matches!(item, StreamItem::BookEvents(_)));
        let item =
            StreamItem::DisconnectedTemporarily(tsclientlib::TemporaryDisconnectReason::Serverstop);
        assert!(!matches!(item, StreamItem::BookEvents(_)));
    }
}
