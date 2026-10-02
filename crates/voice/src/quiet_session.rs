//! One quiet client's TeamSpeak connection.
//!
//! The client connects in [`TECH_SUPPORT_CHANNEL`], then subscribes to
//! every channel on this same connection (`Server::set_subscribed(true)`,
//! which tsclientlib sends as `channelsubscribeall`). Moves are
//! `clientmove` on that connection. Opus frames are handed to
//! `send_audio` only while the director says the client is in the
//! caller's channel with a song open.

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
    ListedClient, LiveQuiet, QuietInstruction, SlotLaunch, SummonDirector, TECH_SUPPORT_CHANNEL,
    cold_audio_source, mint_quiet_identity,
};

pub(crate) async fn run(
    director: SummonDirector,
    launch: SlotLaunch,
    live: LiveQuiet,
) -> Result<()> {
    let server = launch.server.clone();
    let slot = launch.slot;
    let mut stop = launch.stop;
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
    let own = wait_own(&mut con).await?;
    subscribe_all(&mut con).context("subscribe channels")?;
    if let Some((at, clients)) = snapshot(&con) {
        director.mark_ready(&server, slot, at, &clients);
    }

    let mut current: Option<ActiveAudio> = None;
    let mut frames: Option<mpsc::Receiver<AudioMsg>> = None;
    let mut monitor = SendTimingMonitor::new();
    let volume = VolumeHandle::default();

    loop {
        if *stop.borrow() {
            break;
        }
        if let Some(active) = current.as_ref() {
            active.set_paused(!director.playback_open(&server, slot));
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
                    Some(Ok(item)) => {
                        let instr = instructions_for(&director, &server, slot, own, &con, &item);
                        apply(
                            &director,
                            &live,
                            &server,
                            slot,
                            &mut con,
                            &mut current,
                            &mut frames,
                            &volume,
                            instr,
                        )
                        .await;
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
                        if director.offer_frame(&server, slot)
                            && let Err(err) = audio::send_opus_frame(
                                &mut con,
                                &bytes,
                                enqueued_at,
                                &mut monitor,
                                false,
                            )
                        {
                            warn!(%server, slot, error = %err, "quiet client send_audio failed");
                            let instr = director.on_playback_finished(&server, slot);
                            apply(
                                &director,
                                &live,
                                &server,
                                slot,
                                &mut con,
                                &mut current,
                                &mut frames,
                                &volume,
                                instr,
                            )
                            .await;
                        }
                    }
                    Some(AudioMsg::Finished) | None => {
                        frames = None;
                        let instr = director.on_playback_finished(&server, slot);
                        apply(
                            &director,
                            &live,
                            &server,
                            slot,
                            &mut con,
                            &mut current,
                            &mut frames,
                            &volume,
                            instr,
                        )
                        .await;
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
    own: ClientId,
    con: &Connection,
    item: &StreamItem,
) -> Vec<QuietInstruction> {
    let Some((at, clients)) = snapshot(con) else {
        return Vec::new();
    };
    let mut out = director.on_book(server, slot, at, &clients);
    let StreamItem::BookEvents(events) = item else {
        return out;
    };
    for event in events {
        let BookEvent::Message {
            target: MessageTarget::Channel,
            invoker,
            message,
        } = event
        else {
            continue;
        };
        if invoker.id == own {
            continue;
        }
        out.extend(director.on_chat(server, slot, invoker.id.0, message, &clients));
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn apply(
    director: &SummonDirector,
    live: &LiveQuiet,
    server: &str,
    slot: u32,
    con: &mut Connection,
    current: &mut Option<ActiveAudio>,
    frames: &mut Option<mpsc::Receiver<AudioMsg>>,
    volume: &VolumeHandle,
    instrs: Vec<QuietInstruction>,
) {
    for instr in instrs {
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
            }
        }
        if let Some(arg) = instr.play
            && director.playback_open(server, slot)
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
                    let back = director.on_playback_finished(server, slot);
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
