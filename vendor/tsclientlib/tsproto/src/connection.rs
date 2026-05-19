use std::collections::VecDeque;
use std::io;
use std::mem;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::prelude::*;
use generic_array::GenericArray;
use generic_array::typenum::consts::U16;
use num_traits::ToPrimitive;
use tokio::io::ReadBuf;
use tokio::net::UdpSocket;
use tracing::{Span, info_span};
use tsproto_packets::packets::*;
use tsproto_types::crypto::EccKeyPubP256;

use crate::packet_codec::PacketCodec;
use crate::resend::{PacketId, PartialPacketId, Resender, ResenderState};
use crate::{Error, MAX_UDP_PACKET_LENGTH, Result, UDP_SINK_CAPACITY};

/// The needed functions, this can be used to abstract from the underlying
/// transport and allows simulation.
pub trait Socket {
	fn poll_recv_from(&self, cx: &mut Context, buf: &mut ReadBuf) -> Poll<io::Result<SocketAddr>>;
	fn poll_send_to(
		&self, cx: &mut Context, buf: &[u8], target: SocketAddr,
	) -> Poll<io::Result<usize>>;
	fn local_addr(&self) -> io::Result<SocketAddr>;

	/// PURA-403 fix — duplicate the underlying fd into a second
	/// `std::net::UdpSocket` handle so ack sends can be offloaded to a
	/// background thread.  The default returns `None` (test simulated sockets,
	/// non-unix builds).  Only `impl Socket for UdpSocket` overrides this.
	#[cfg(unix)]
	fn try_clone_socket(&self) -> Option<std::net::UdpSocket> { None }
}

/// A cache for the key and nonce for a generation id.
/// This has to be stored for each packet type.
#[derive(Debug)]
pub struct CachedKey {
	pub generation_id: u32,
	pub key: GenericArray<u8, U16>,
	pub nonce: GenericArray<u8, U16>,
}

/// Data that has to be stored for a connection when it is connected.
pub struct ConnectedParams {
	/// The client id of this connection.
	pub c_id: u16,
	/// If voice packets should be encrypted
	pub voice_encryption: bool,

	/// The public key of the other side.
	pub public_key: EccKeyPubP256,
	/// The iv used to encrypt and decrypt packets.
	pub shared_iv: [u8; 64],
	/// The mac used for unencrypted packets.
	pub shared_mac: [u8; 8],
	/// Cached key and nonce per packet type and for server to client (without
	/// client id inside the packet) and client to server communication.
	pub key_cache: [[CachedKey; 2]; 8],
}

/// An event that originates from a tsproto raw connection.
#[derive(Debug)]
pub enum Event<'a> {
	ReceiveUdpPacket(&'a InUdpPacket<'a>),
	ReceivePacket(&'a InPacket<'a>),
	SendUdpPacket(&'a OutUdpPacket),
	SendPacket(&'a OutPacket),
}

/// An item that originates from a tsproto raw event stream.
///
/// The disconnected event is signaled by returning `None` from the stream.
#[derive(Debug)]
pub enum StreamItem {
	Command(InCommandBuf),
	Audio(InAudioBuf),
	C2SInit(InC2SInitBuf),
	S2CInit(InS2CInitBuf),
	/// All packets with an id less or equal to this id were acknowledged.
	AckPacket(PacketId),
	/// The network statistics were updated.
	NetworkStatsUpdated,
	Error(Error),
}

type EventListener = Box<dyn for<'a> Fn(&'a Event<'a>) + Send>;

/// Represents a currently alive connection.
pub struct Connection {
	pub is_client: bool,
	pub span: Span,
	/// The parameters of this connection, if it is already established.
	pub params: Option<ConnectedParams>,
	/// The address of the other side, where packets are coming from and going
	/// to.
	pub address: SocketAddr,

	pub resender: Resender,
	pub codec: PacketCodec,
	pub udp_socket: Box<dyn Socket + Send>,
	udp_buffer: Vec<u8>,

	/// A buffer of packets that should be returned from the stream.
	///
	/// If a new udp packet is received and we already received the following
	/// ids, we can get multiple packets back at once. As we can only return one
	/// from the stream, the rest is stored here.
	pub(crate) stream_items: VecDeque<StreamItem>,

	/// The queue of non-command packets that should be sent.
	///
	/// These packets are not influenced by congestion control.
	/// If it gets too long, we don't poll from the `udp_socket` anymore.
	acks_to_send: VecDeque<OutUdpPacket>,

	/// PURA-403 fix — channel to the background ack-sender thread.
	///
	/// Ack sends (`poll_send_acks`) were blocking the connected loop for 8–57 ms
	/// each (confirmed `wall ≈ cpu` on 90% of 425 stall events — genuine kernel
	/// work, not preemption). By offloading to a dedicated thread the connected
	/// loop — and thus the 20 ms audio cadence — is never stalled by an ack send.
	///
	/// `None` on non-unix builds and when the socket does not support fd-dup
	/// (e.g. test `SimulatedSocket`), in which case we fall back to the original
	/// in-line send path.
	ack_thread_tx: Option<std::sync::mpsc::SyncSender<(Vec<u8>, SocketAddr)>>,

	pub event_listeners: Vec<EventListener>,
}

impl Socket for UdpSocket {
	fn poll_recv_from(&self, cx: &mut Context, buf: &mut ReadBuf) -> Poll<io::Result<SocketAddr>> {
		self.poll_recv_from(cx, buf)
	}

	fn poll_send_to(
		&self, cx: &mut Context, buf: &[u8], target: SocketAddr,
	) -> Poll<io::Result<usize>> {
		self.poll_send_to(cx, buf, target)
	}

	fn local_addr(&self) -> io::Result<SocketAddr> { self.local_addr() }

	/// PURA-403 — dup the tokio UdpSocket fd so the background ack-sender thread
	/// can call `send_to` without touching the tokio reactor at all.
	///
	/// IMPORTANT: `dup(2)` shares the *open file description* — `O_NONBLOCK` is a
	/// property of that description, not the fd, so the dup is non-blocking too
	/// and **must stay that way**. Calling `set_nonblocking(false)` here would
	/// clear `O_NONBLOCK` on the description the tokio reactor still uses,
	/// turning the connected loop's `poll_recv_from` into a blocking `recvfrom`
	/// that freezes the whole voice runtime between inbound packets. The
	/// ack-sender thread therefore does a non-blocking `send_to`; a rare
	/// `WouldBlock` simply drops the ack (the resender retransmits the command).
	#[cfg(unix)]
	fn try_clone_socket(&self) -> Option<std::net::UdpSocket> {
		use std::os::unix::io::{AsRawFd, FromRawFd};
		// SAFETY: `dup(2)` is always safe on a valid fd; `from_raw_fd` on the
		// resulting fd is safe because we just created it and take ownership.
		let dup_fd = unsafe { libc::dup(self.as_raw_fd()) };
		if dup_fd < 0 {
			return None;
		}
		// SAFETY: `dup_fd` is freshly created and owned exclusively here.
		Some(unsafe { std::net::UdpSocket::from_raw_fd(dup_fd) })
	}
}

impl Default for CachedKey {
	fn default() -> Self {
		CachedKey { generation_id: u32::MAX, key: [0; 16].into(), nonce: [0; 16].into() }
	}
}

impl ConnectedParams {
	/// Fills the parameters for a connection with their default state.
	pub fn new(public_key: EccKeyPubP256, shared_iv: [u8; 64], shared_mac: [u8; 8]) -> Self {
		Self {
			c_id: 0,
			voice_encryption: true,
			public_key,
			shared_iv,
			shared_mac,
			key_cache: Default::default(),
		}
	}
}

impl Connection {
	pub fn new(is_client: bool, address: SocketAddr, udp_socket: Box<dyn Socket + Send>) -> Self {
		let span = info_span!("connection", local_addr = %udp_socket.local_addr().unwrap(),
			remote_addr = %address);

		// PURA-403 fix — spawn an ack-sender thread when the socket supports
		// fd-dup (tokio UdpSocket on unix). The thread owns a dup'd handle of
		// the socket and drains a bounded channel, calling `send_to` on each ack
		// packet. The dup shares the non-blocking open file description, so the
		// `send_to` is non-blocking — a rare `WouldBlock` drops the ack. Capacity
		// 256 matches `UDP_SINK_CAPACITY`; try_send in `poll_send_acks` likewise
		// drops excess acks (UDP semantics — the resender retransmits).
		#[cfg(unix)]
		let ack_thread_tx = udp_socket.try_clone_socket().map(|sock| {
			let (tx, rx) =
				std::sync::mpsc::sync_channel::<(Vec<u8>, SocketAddr)>(256);
			std::thread::Builder::new()
				.name("tsproto-ack-sender".into())
				.spawn(move || {
					while let Ok((data, addr)) = rx.recv() {
						let _ = sock.send_to(&data, addr);
					}
				})
				.expect("spawn tsproto-ack-sender");
			tx
		});
		#[cfg(not(unix))]
		let ack_thread_tx: Option<std::sync::mpsc::SyncSender<(Vec<u8>, SocketAddr)>> = None;

		let mut res = Self {
			is_client,
			span,
			params: None,
			address,
			resender: Default::default(),
			codec: Default::default(),
			udp_socket,
			udp_buffer: Default::default(),

			stream_items: Default::default(),
			acks_to_send: Default::default(),
			ack_thread_tx,
			event_listeners: Default::default(),
		};
		if is_client {
			// The first command is sent as part of the C2SInit::Init4 packet
			// so it does not get registered automatically.
			res.codec.outgoing_p_ids[PacketType::Command.to_usize().unwrap()] =
				PartialPacketId { generation_id: 0, packet_id: 1 };
		} else {
			res.codec.incoming_p_ids[PacketType::Command.to_usize().unwrap()] =
				PartialPacketId { generation_id: 0, packet_id: 1 };
		}
		res
	}

	/// Check if a given id is in the receive window.
	///
	/// Returns
	/// 1. If the packet id is inside the receive window
	/// 1. The generation of the packet
	/// 1. The minimum accepted packet id
	/// 1. The maximum accepted packet id
	pub(crate) fn in_receive_window(&self, p_type: PacketType, p_id: u16) -> (bool, u32, u16, u16) {
		if p_type == PacketType::Init {
			return (true, 0, 0, 0);
		}
		let type_i = p_type.to_usize().unwrap();
		// Receive window is the next half of ids
		let cur_next = self.codec.incoming_p_ids[type_i].packet_id;
		let (limit, next_gen) = cur_next.overflowing_add(u16::MAX / 2);
		let gen = self.codec.incoming_p_ids[type_i].generation_id;
		let in_recv_win = (!next_gen && p_id >= cur_next && p_id < limit)
			|| (next_gen && (p_id >= cur_next || p_id < limit));
		let gen_id = if in_recv_win {
			if next_gen && p_id < limit { gen + 1 } else { gen }
		} else if p_id < cur_next {
			gen
		} else {
			gen - 1
		};

		(in_recv_win, gen_id, cur_next, limit)
	}

	pub fn send_event(&self, event: &Event) {
		for l in &self.event_listeners {
			l(event)
		}
	}

	pub fn hand_back_buffer(&mut self, buffer: Vec<u8>) {
		if self.udp_buffer.capacity() < MAX_UDP_PACKET_LENGTH
			&& buffer.capacity() >= MAX_UDP_PACKET_LENGTH
		{
			self.udp_buffer = buffer;
		}
	}

	/// Flush the queued ack packets onto the wire.
	///
	/// Returns the number of acks actually flushed this call. PURA-403 — when
	/// the background ack-sender thread is available (`ack_thread_tx.is_some()`)
	/// this path is O(queue_depth) channel-pushes and never calls `poll_send_to`
	/// itself, so it cannot stall the connected loop regardless of kernel send
	/// latency. The thread drains the channel with blocking `send_to` calls on
	/// a dedicated OS thread, isolated from the voice-rt workers.
	fn poll_send_acks(&mut self, cx: &mut Context) -> Result<usize> {
		let mut flushed = 0usize;

		// PURA-403 fast path — channel-push to the background ack-sender thread.
		// `try_send` is non-blocking; if the channel is full (256 cap) we drop the
		// ack (UDP reliability: the remote will retransmit the unack'd command).
		if let Some(tx) = &self.ack_thread_tx {
			while let Some(packet) = self.acks_to_send.front() {
				let data = packet.data().data().to_vec();
				// Fire the event while we still hold the packet reference.
				self.send_event(&Event::SendUdpPacket(packet));
				self.resender.handle_loss_outgoing(packet);
				self.acks_to_send.pop_front();
				// Drop excess acks rather than blocking the connected loop.
				let _ = tx.try_send((data, self.address));
				flushed += 1;
			}
			return Ok(flushed);
		}

		// Fallback — original inline send path (non-unix, tests).
		while let Some(packet) = self.acks_to_send.front() {
			match self.poll_send_udp_packet(cx, packet) {
				Poll::Ready(Ok(())) => {
					self.resender.handle_loss_outgoing(packet);
				}
				Poll::Ready(Err(e)) => return Err(e),
				Poll::Pending => break,
			}
			self.acks_to_send.pop_front();
			flushed += 1;
		}
		Ok(flushed)
	}

	fn poll_incoming_udp_packet(&mut self, cx: &mut Context) -> Poll<Result<StreamItem>> {
		if self.acks_to_send.len() >= UDP_SINK_CAPACITY {
			return Poll::Pending;
		}

		loop {
			// Poll udp_socket
			if self.udp_buffer.len() != MAX_UDP_PACKET_LENGTH {
				self.udp_buffer.resize(MAX_UDP_PACKET_LENGTH, 0);
			}

			let mut read_buf = ReadBuf::new(&mut self.udp_buffer);
			match self.udp_socket.poll_recv_from(cx, &mut read_buf) {
				Poll::Ready(Ok(addr)) => {
					let size = read_buf.filled().len();
					let mut udp_buffer = mem::take(&mut self.udp_buffer);
					udp_buffer.truncate(size);
					match self.handle_udp_packet(cx, udp_buffer, addr) {
						Ok(()) => {
							if let Some(item) = self.stream_items.pop_front() {
								return Poll::Ready(Ok(item));
							}
						}
						Err(e) => {
							return Poll::Ready(Err(e));
						}
					}
				}
				// Udp socket closed
				Poll::Ready(Err(e)) => return Poll::Ready(Err(Error::Network(e))),
				Poll::Pending => return Poll::Pending,
			}
		}
	}

	fn handle_udp_packet(
		&mut self, cx: &mut Context, udp_buffer: Vec<u8>, addr: SocketAddr,
	) -> Result<()> {
		let _span = self.span.clone().entered();
		if addr != self.address {
			self.stream_items.push_back(StreamItem::Error(Error::WrongAddress));
			return Ok(());
		}

		let dir = if self.is_client { Direction::S2C } else { Direction::C2S };
		let packet = InUdpPacket(match InPacket::try_new(dir, &udp_buffer) {
			Ok(r) => r,
			Err(e) => {
				self.stream_items.push_back(StreamItem::Error(Error::PacketParse("udp", e)));
				return Ok(());
			}
		});
		let event = Event::ReceiveUdpPacket(&packet);
		self.send_event(&event);

		self.resender.received_packet();
		PacketCodec::handle_udp_packet(self, cx, udp_buffer)?;

		Ok(())
	}

	/// Try to send an ack packet.
	///
	/// If it does not work, add it to the ack queue.
	pub(crate) fn send_ack_packet(&mut self, cx: &mut Context, packet: OutPacket) -> Result<()> {
		self.send_event(&Event::SendPacket(&packet));
		let mut udp_packets = PacketCodec::encode_packet(self, packet)?;
		assert_eq!(
			udp_packets.len(),
			1,
			"Encoding an ack packet should only yield a single packet"
		);
		let packet = udp_packets.pop().unwrap();

		match self.poll_send_udp_packet(cx, &packet) {
			Poll::Ready(r) => {
				if r.is_ok() {
					self.resender.handle_loss_outgoing(&packet);
				}
				r
			}
			Poll::Pending => {
				self.acks_to_send.push_back(packet);
				Ok(())
			}
		}
	}

	/// Add a packet to the send queue.
	///
	/// This function buffers indefinitely, to prevent using a large amount of
	/// memory, check `is_send_queue_full` first and only send a packet if this
	/// function returns `false`.
	///
	/// When the `PacketId` which is returned by this function is acknowledged,
	/// the packet was successfully received by the other side of the
	/// connection.
	pub fn send_packet(&mut self, packet: OutPacket) -> Result<PacketId> {
		self.send_event(&Event::SendPacket(&packet));
		let udp_packets = PacketCodec::encode_packet(self, packet)?;

		let id = udp_packets.last().unwrap().into();
		for p in udp_packets {
			self.send_udp_packet(p);
		}
		Ok(id)
	}

	/// Add an udp packet to the send queue.
	pub fn send_udp_packet(&mut self, packet: OutUdpPacket) {
		let _span = self.span.clone().entered();
		match packet.packet_type() {
			PacketType::Init | PacketType::Command | PacketType::CommandLow => {
				Resender::send_packet(self, packet);
			}
			_ => self.acks_to_send.push_back(packet),
		}
	}

	pub fn poll_send_udp_packet(
		&self, cx: &mut Context, packet: &OutUdpPacket,
	) -> Poll<Result<()>> {
		Self::static_poll_send_udp_packet(
			&*self.udp_socket,
			self.address,
			&self.event_listeners,
			cx,
			packet,
		)
	}

	/// Remember to add the size of the sent packet to the stats in the resender.
	pub fn static_poll_send_udp_packet(
		udp_socket: &dyn Socket, address: SocketAddr, event_listeners: &[EventListener],
		cx: &mut Context, packet: &OutUdpPacket,
	) -> Poll<Result<()>> {
		let data = packet.data().data();
		match udp_socket.poll_send_to(cx, data, address).map_err(Error::Network)? {
			Poll::Pending => Poll::Pending,
			Poll::Ready(size) => {
				let event = Event::SendUdpPacket(packet);
				for l in event_listeners {
					l(&event)
				}

				if size != data.len() {
					Poll::Ready(Err(Error::Network(std::io::Error::new(
						std::io::ErrorKind::Other,
						"Failed to send whole udp packet",
					))))
				} else {
					Poll::Ready(Ok(()))
				}
			}
		}
	}

	pub fn is_send_queue_full(&self) -> bool { self.resender.is_full() }
	pub fn is_send_queue_empty(&self) -> bool { self.resender.is_empty() }
}

/// PURA-403 — thread CPU time consumed by the *calling* thread so far, via
/// `CLOCK_THREAD_CPUTIME_ID`.
///
/// [`PollLegTimer`] measures wall-clock elapsed, which structurally cannot tell
/// apart two very different stalls (CTO review on PURA-403):
///
/// * `wall ≈ cpu` — the thread was genuinely on-CPU the whole leg (busy work or
///   a blocking syscall). The decouple-ack-sends fix is warranted.
/// * `wall ≫ cpu` — the thread was *descheduled* (preempted) mid-leg. Moving
///   the work to another task on the same runtime would not help; this routes
///   to the voice-runtime scheduling work in PURA-367.
///
/// Bracketing a leg with this lets the WARN log the off-CPU fraction so the
/// freeze is *diagnosed*, not guessed.
#[cfg(unix)]
fn thread_cpu_time() -> std::time::Duration {
	let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
	// SAFETY: `ts` is a valid, writable `timespec`; `CLOCK_THREAD_CPUTIME_ID` is
	// a valid POSIX clock id. `clock_gettime` only writes `ts` and returns 0 on
	// success — on the (not expected) error path we fall back to ZERO.
	let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
	if rc != 0 {
		return std::time::Duration::ZERO;
	}
	std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

#[cfg(not(unix))]
fn thread_cpu_time() -> std::time::Duration { std::time::Duration::ZERO }

/// PURA-403 — per-leg timing for one [`Connection::poll_next`] call.
///
/// The voice connected loop (`crates/voice`) drives this `Stream` on a
/// `tokio::select!` arm. The select runs every arm's future on one task, so a
/// *synchronous* stall anywhere inside `poll_next` freezes the whole voice
/// runtime — including the 20 ms audio cadence — for the stall's duration.
/// PURA-399 caught 25–87 ms `connected_loop_stall arm=audio` events with no
/// companion per-frame `audio_send_attribution`: the freeze is structurally
/// invisible to the send-path instrumentation because it happens while the
/// select is polling *this* stream, not the audio arm.
///
/// This guard times each leg of `poll_next` and, via `Drop`, emits exactly
/// one WARN (`target: "voice_poll_leg"`) when the combined poll crosses
/// [`POLL_LEG_WARN`] — so the freeze can be *attributed* to a concrete leg
/// (PURA-403 requirement: measure the leg, do not guess it). `Drop` covers
/// every early-return path in `poll_next` without threading a log call
/// through each one.
struct PollLegTimer {
	start: std::time::Instant,
	/// Thread CPU time at `poll_next` entry — see [`thread_cpu_time`].
	cpu_start: std::time::Duration,
	send_acks: std::time::Duration,
	/// Thread CPU time consumed *by the `send_acks` leg* (the PURA-399 suspect).
	send_acks_cpu: std::time::Duration,
	send_acks_flushed: usize,
	resend: std::time::Duration,
	ping: std::time::Duration,
	incoming: std::time::Duration,
}

/// Combined-poll duration above which [`PollLegTimer`] logs a leg breakdown.
/// Well under the 20 ms audio frame budget so any stall that can drop a frame
/// is captured, while steady-state sub-millisecond polls stay silent.
const POLL_LEG_WARN: std::time::Duration = std::time::Duration::from_millis(8);

impl Drop for PollLegTimer {
	fn drop(&mut self) {
		let total = self.start.elapsed();
		if total >= POLL_LEG_WARN {
			// PURA-403 finding-#5 disambiguation. `off_cpu = wall − cpu`: a
			// large off-CPU fraction means the voice-rt thread was descheduled
			// mid-poll (preemption ⇒ PURA-367), not stuck in a syscall.
			let total_cpu = thread_cpu_time().saturating_sub(self.cpu_start);
			let off_cpu = total.saturating_sub(total_cpu);
			tracing::warn!(
				target: "voice_poll_leg",
				total_us = total.as_micros() as u64,
				total_cpu_us = total_cpu.as_micros() as u64,
				off_cpu_us = off_cpu.as_micros() as u64,
				send_acks_us = self.send_acks.as_micros() as u64,
				send_acks_cpu_us = self.send_acks_cpu.as_micros() as u64,
				send_acks_flushed = self.send_acks_flushed,
				resend_us = self.resend.as_micros() as u64,
				ping_us = self.ping.as_micros() as u64,
				incoming_us = self.incoming.as_micros() as u64,
				"PURA-403 connection poll_next leg stall — synchronous freeze of the voice runtime"
			);
		}
	}
}

/// Pull for events.
///
/// `Ok(StreamItem::Error)` is recoverable, `Err()` is not.
///
/// Polling does a few things in round robin fashion:
/// 1. Check for new udp packets
/// 2. Use the resender to resend packets if necessary
/// 3. Use the resender to send ping packets if necessary
impl Stream for Connection {
	type Item = Result<StreamItem>;
	fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
		let _span = self.span.clone().entered();
		// PURA-403 — `timer` times each leg below and logs a breakdown on
		// `Drop` if the combined poll stalls. `Drop` fires on every return
		// path, so no early return needs to be touched.
		let mut timer = PollLegTimer {
			start: std::time::Instant::now(),
			cpu_start: thread_cpu_time(),
			send_acks: std::time::Duration::ZERO,
			send_acks_cpu: std::time::Duration::ZERO,
			send_acks_flushed: 0,
			resend: std::time::Duration::ZERO,
			ping: std::time::Duration::ZERO,
			incoming: std::time::Duration::ZERO,
		};

		let leg = std::time::Instant::now();
		let leg_cpu = thread_cpu_time();
		match self.poll_send_acks(cx) {
			Ok(flushed) => timer.send_acks_flushed = flushed,
			Err(e) => return Poll::Ready(Some(Err(e))),
		}
		timer.send_acks = leg.elapsed();
		timer.send_acks_cpu = thread_cpu_time().saturating_sub(leg_cpu);

		if self.resender.get_state() == ResenderState::Disconnected {
			// Send all ack packets and return `None` afterwards
			if self.acks_to_send.is_empty() {
				return Poll::Ready(None);
			}
		}

		// Use the resender to resend packets
		let leg = std::time::Instant::now();
		match Resender::poll_resend(&mut self, cx) {
			Ok(()) => {}
			Err(e) => return Poll::Ready(Some(Err(e))),
		}
		timer.resend = leg.elapsed();

		// Use the resender to send pings
		let leg = std::time::Instant::now();
		match Resender::poll_ping(&mut self, cx) {
			Ok(()) => {}
			Err(e) => return Poll::Ready(Some(Err(e))),
		}
		timer.ping = leg.elapsed();

		// Return existing stream_items
		if let Some(item) = self.stream_items.pop_front() {
			return Poll::Ready(Some(Ok(item)));
		}

		// Check for new udp packets
		let leg = std::time::Instant::now();
		let incoming = self.poll_incoming_udp_packet(cx);
		timer.incoming = leg.elapsed();
		match incoming {
			Poll::Ready(r) => return Poll::Ready(Some(r)),
			Poll::Pending => {}
		}

		Poll::Pending
	}
}
