use core::num::NonZeroU32;
use core::time::Duration;

use crate::Instant;
use crate::wire::{AckInfo, Body, DATA_HEADER_LEN, Frame};

/// Timing and retry settings for a [`Link`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
	/// How long to wait for an acknowledgement before the first resend.
	/// Each further resend waits twice as long as the one before.
	pub retransmit_timeout: Duration,
	/// The longest wait between resends.
	pub max_retransmit_timeout: Duration,
	/// How many times to resend a message before giving up on the link.
	pub max_retransmits: u8,
	/// How often to send a handshake frame while the peer has not answered.
	pub hello_interval: Duration,
	/// The longest a frame can spend between being sent and arriving. After
	/// the peer restarts, frames from its old session are ignored for this
	/// long. If a frame arrives later than this, the link may mistake it for
	/// a new restart and lose data.
	pub max_frame_age: Duration,
}

impl Default for Config {
	fn default() -> Self {
		Config {
			retransmit_timeout: Duration::from_millis(50),
			max_retransmit_timeout: Duration::from_secs(1),
			max_retransmits: 8,
			hello_interval: Duration::from_millis(500),
			max_frame_age: Duration::from_millis(100),
		}
	}
}

/// Something the application should know about. See [`Link::poll_event`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
	/// Both sides know each other, and queued messages are being sent.
	Connected,
	/// The peer restarted. Messages it had not acknowledged were discarded,
	/// and the link is handshaking again. Anything the application built on
	/// the old connection should be thrown away.
	Reset,
	/// A message went unacknowledged after every resend. The link is now
	/// closed for good. Create a new one with a new session id to try again.
	Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
	/// The send window is full. Wait for the peer to acknowledge something.
	Full,
	/// The payload does not fit in one frame. See [`Link::max_payload`].
	TooLarge,
	/// The link has failed.
	Closed,
}

impl core::fmt::Display for SendError {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(match self {
			SendError::Full => "send window is full",
			SendError::TooLarge => "payload does not fit in one frame",
			SendError::Closed => "link has failed",
		})
	}
}

impl core::error::Error for SendError {}

const CONNECTED: u8 = 1;
const RESET: u8 = 2;
const FAILED: u8 = 4;

/// How many times a message is resent right away after [`Link::frame_lost`],
/// before resends fall back to the timer.
const FAST_RESENDS: u8 = 2;

#[derive(Clone, Copy)]
struct SendSlot<const N: usize> {
	data: [u8; N],
	len: usize,
	/// When to resend, or `None` if never sent.
	deadline: Option<Instant>,
	transmissions: u8,
	acked: bool,
}

#[derive(Clone, Copy)]
struct RecvSlot<const N: usize> {
	data: [u8; N],
	len: usize,
	filled: bool,
}

/// One end of a reliable, ordered message stream with one peer.
///
/// `WINDOW` is how many messages may be in flight at once. It must be a power
/// of two no larger than 32. `N` is the largest datagram the radio can send,
/// such as [`MAX_DATA_LEN_V2`](crate::MAX_DATA_LEN_V2). A link holds two
/// buffers of `N` bytes per window slot, so it is large: keep it in a static
/// or on the heap rather than on a small stack.
pub struct Link<const WINDOW: usize = 8, const N: usize = { crate::MAX_DATA_LEN_V2 }> {
	config: Config,
	local: NonZeroU32,
	remote: Option<NonZeroU32>,
	remote_since: Instant,
	peer_knows_us: bool,
	failed: bool,
	events: u8,
	hello_at: Option<Instant>,
	ack_pending: bool,
	send_base: u16,
	send_next: u16,
	send_slots: [SendSlot<N>; WINDOW],
	recv_next: u16,
	recv_slots: [RecvSlot<N>; WINDOW],
}

impl<const WINDOW: usize, const N: usize> Link<WINDOW, N> {
	const VALID: () = {
		assert!(
			WINDOW.is_power_of_two() && WINDOW <= 32,
			"WINDOW must be a power of two up to 32"
		);
		assert!(N > DATA_HEADER_LEN, "N is too small to hold a data frame");
	};

	/// Creates a link that starts handshaking right away. `session` must be
	/// random and different every time a link is created, including across
	/// reboots, or the peer cannot tell that this side restarted.
	pub fn new(session: NonZeroU32, config: Config) -> Self {
		let () = Self::VALID;
		Link {
			config,
			local: session,
			remote: None,
			remote_since: Instant::ZERO,
			peer_knows_us: false,
			failed: false,
			events: 0,
			hello_at: Some(Instant::ZERO),
			ack_pending: false,
			send_base: 0,
			send_next: 0,
			send_slots: [SendSlot {
				data: [0; N],
				len: 0,
				deadline: None,
				transmissions: 0,
				acked: false,
			}; WINDOW],
			recv_next: 0,
			recv_slots: [RecvSlot {
				data: [0; N],
				len: 0,
				filled: false,
			}; WINDOW],
		}
	}

	/// The largest payload [`Link::send`] accepts.
	pub const fn max_payload() -> usize {
		N - DATA_HEADER_LEN
	}

	pub fn session(&self) -> NonZeroU32 {
		self.local
	}

	/// The peer's current session id, once a frame from it has arrived.
	pub fn peer_session(&self) -> Option<NonZeroU32> {
		self.remote
	}

	/// Whether both sides know each other and messages are flowing.
	pub fn is_connected(&self) -> bool {
		!self.failed && self.remote.is_some() && self.peer_knows_us
	}

	pub fn is_failed(&self) -> bool {
		self.failed
	}

	/// How many more messages [`Link::send`] will accept right now.
	pub fn send_capacity(&self) -> usize {
		if self.failed {
			return 0;
		}
		WINDOW - self.in_flight()
	}

	/// Whether every message sent so far has been acknowledged.
	pub fn is_idle(&self) -> bool {
		self.in_flight() == 0
	}

	fn in_flight(&self) -> usize {
		usize::from(self.send_next.wrapping_sub(self.send_base))
	}

	/// Queues a message. It may be sent before the handshake finishes and is
	/// delivered exactly once and in order, unless an [`Event::Reset`] or
	/// [`Event::Failed`] comes first.
	pub fn send(&mut self, payload: &[u8]) -> Result<(), SendError> {
		if self.failed {
			return Err(SendError::Closed);
		}
		if payload.len() > Self::max_payload() {
			return Err(SendError::TooLarge);
		}
		if self.in_flight() == WINDOW {
			return Err(SendError::Full);
		}
		let slot = &mut self.send_slots[slot_index::<WINDOW>(self.send_next)];
		slot.data[..payload.len()].copy_from_slice(payload);
		slot.len = payload.len();
		slot.deadline = None;
		slot.transmissions = 0;
		slot.acked = false;
		self.send_next = self.send_next.wrapping_add(1);
		Ok(())
	}

	/// Returns the next thing the application should know about.
	pub fn poll_event(&mut self) -> Option<Event> {
		for (flag, event) in [
			(RESET, Event::Reset),
			(CONNECTED, Event::Connected),
			(FAILED, Event::Failed),
		] {
			if self.events & flag != 0 {
				self.events &= !flag;
				return Some(event);
			}
		}
		None
	}

	/// When [`Link::poll_transmit`] next has something to send. The time may be
	/// in the past, which means right away.
	pub fn poll_timeout(&self) -> Option<Instant> {
		if self.failed {
			return None;
		}
		let mut earliest = self.hello_at;
		if self.is_connected() {
			if self.ack_pending {
				return Some(Instant::ZERO);
			}
			for slot in self.unacked_slots() {
				let due = slot.deadline.unwrap_or(Instant::ZERO);
				earliest = Some(earliest.map_or(due, |current| current.min(due)));
			}
		}
		earliest
	}

	fn unacked_slots(&self) -> impl Iterator<Item = &SendSlot<N>> {
		(0..self.in_flight() as u16)
			.map(|offset| {
				&self.send_slots[slot_index::<WINDOW>(self.send_base.wrapping_add(offset))]
			})
			.filter(|slot| !slot.acked)
	}

	/// Writes the next datagram to send into `buf`, which must hold at least
	/// `N` bytes, and returns its length. Call this until it returns `None`
	/// after every call to [`Link::send`] or [`Link::handle`], and whenever
	/// [`Link::poll_timeout`] passes.
	pub fn poll_transmit(&mut self, now: Instant, buf: &mut [u8]) -> Option<usize> {
		if self.failed {
			return None;
		}
		if self.hello_at.is_some_and(|at| at <= now) {
			self.hello_at = (!self.is_connected()).then(|| now + self.config.hello_interval);
			return Some(self.encode(Body::Hello, buf));
		}
		if !self.is_connected() {
			return None;
		}
		if let Some(seq) = self.due_seq(now) {
			return self.transmit_data(seq, now, buf);
		}
		if self.ack_pending {
			self.ack_pending = false;
			return Some(self.encode(Body::Ack(self.ack_info()), buf));
		}
		None
	}

	fn due_seq(&self, now: Instant) -> Option<u16> {
		(0..self.in_flight() as u16)
			.map(|offset| self.send_base.wrapping_add(offset))
			.find(|&seq| {
				let slot = &self.send_slots[slot_index::<WINDOW>(seq)];
				!slot.acked && slot.deadline.is_none_or(|deadline| deadline <= now)
			})
	}

	fn transmit_data(&mut self, seq: u16, now: Instant, buf: &mut [u8]) -> Option<usize> {
		let index = slot_index::<WINDOW>(seq);
		let transmissions = self.send_slots[index].transmissions;
		if transmissions > self.config.max_retransmits {
			self.failed = true;
			self.events |= FAILED;
			return None;
		}
		let slot = &mut self.send_slots[index];
		slot.transmissions += 1;
		slot.deadline = Some(now + retransmit_delay(&self.config, transmissions));
		self.ack_pending = false;
		let slot = &self.send_slots[index];
		let frame = Frame {
			src: self.local,
			dst: self.remote,
			body: Body::Data {
				seq,
				ack: self.ack_info(),
				payload: &slot.data[..slot.len],
			},
		};
		Some(frame.encode(buf))
	}

	fn encode(&self, body: Body<'_>, buf: &mut [u8]) -> usize {
		Frame {
			src: self.local,
			dst: self.remote,
			body,
		}
		.encode(buf)
	}

	fn ack_info(&self) -> AckInfo {
		let mut sack = 0;
		for offset in 1..WINDOW as u16 {
			if self.recv_slots[slot_index::<WINDOW>(self.recv_next.wrapping_add(offset))].filled {
				sack |= 1 << (offset - 1);
			}
		}
		AckInfo {
			ack: self.recv_next,
			sack,
		}
	}

	/// Tells the link that a datagram from [`Link::poll_transmit`] did not
	/// arrive, such as when ESP-NOW reports that the peer did not acknowledge
	/// it. A message lost this way is resent right away instead of when its
	/// timer runs out, the first couple of times. Other frames are left to
	/// their timers, so a peer that is gone does not cause a flood of resends.
	pub fn frame_lost(&mut self, datagram: &[u8], now: Instant) {
		let Some(frame) = Frame::decode(datagram) else {
			return;
		};
		let Body::Data { seq, .. } = frame.body else {
			return;
		};
		if self.failed || frame.src != self.local || frame.dst != self.remote {
			return;
		}
		if usize::from(seq.wrapping_sub(self.send_base)) >= self.in_flight() {
			return;
		}
		let slot = &mut self.send_slots[slot_index::<WINDOW>(seq)];
		if !slot.acked && slot.transmissions <= FAST_RESENDS {
			slot.deadline = Some(now);
		}
	}

	/// Handles a datagram from the peer, calling `deliver` with each message
	/// that is now ready, in order. Datagrams that are malformed or from an
	/// old session are ignored.
	pub fn handle(&mut self, datagram: &[u8], now: Instant, mut deliver: impl FnMut(&[u8])) {
		if self.failed {
			return;
		}
		let Some(frame) = Frame::decode(datagram) else {
			return;
		};
		if frame.body == Body::Hello {
			self.handle_hello(frame, now);
			return;
		}
		if frame.dst != Some(self.local) {
			// The peer is talking to an earlier link of ours.
			self.hello_at = Some(now);
			return;
		}
		if self.remote != Some(frame.src) {
			return;
		}
		self.peer_knows_us_now();
		match frame.body {
			Body::Hello => {}
			Body::Ack(ack) => self.handle_ack(ack),
			Body::Data { seq, ack, payload } => {
				self.handle_ack(ack);
				self.handle_data(seq, payload, &mut deliver);
			}
		}
	}

	fn handle_hello(&mut self, frame: Frame<'_>, now: Instant) {
		match self.remote {
			Some(remote) if remote == frame.src => {}
			// Possibly a late frame from a session the peer had before this one.
			Some(_) if now < self.remote_since + self.config.max_frame_age => return,
			Some(_) => self.reset(frame.src, now),
			None => {
				self.remote = Some(frame.src);
				self.remote_since = now;
			}
		}
		if frame.dst == Some(self.local) {
			self.peer_knows_us_now();
			// It is still handshaking, so tell it we know it too.
			self.ack_pending = true;
		} else {
			self.hello_at = Some(now);
		}
	}

	fn peer_knows_us_now(&mut self) {
		if !self.peer_knows_us {
			self.peer_knows_us = true;
			self.hello_at = None;
			self.events |= CONNECTED;
		}
	}

	fn reset(&mut self, remote: NonZeroU32, now: Instant) {
		self.remote = Some(remote);
		self.remote_since = now;
		self.peer_knows_us = false;
		self.events = (self.events & !CONNECTED) | RESET;
		self.ack_pending = false;
		self.send_base = 0;
		self.send_next = 0;
		self.recv_next = 0;
		for slot in &mut self.recv_slots {
			slot.filled = false;
		}
	}

	fn handle_ack(&mut self, info: AckInfo) {
		let acked = info.ack.wrapping_sub(self.send_base);
		if usize::from(acked) > self.in_flight() {
			return;
		}
		self.send_base = info.ack;
		for bit in 0..32u16 {
			if info.sack & (1 << bit) == 0 {
				continue;
			}
			let seq = info.ack.wrapping_add(1 + bit);
			if seq.wrapping_sub(self.send_base) < self.send_next.wrapping_sub(self.send_base) {
				self.send_slots[slot_index::<WINDOW>(seq)].acked = true;
			}
		}
	}

	fn handle_data(&mut self, seq: u16, payload: &[u8], deliver: &mut impl FnMut(&[u8])) {
		self.ack_pending = true;
		let offset = usize::from(seq.wrapping_sub(self.recv_next));
		if offset >= WINDOW || payload.len() > Self::max_payload() {
			return;
		}
		if offset > 0 {
			let slot = &mut self.recv_slots[slot_index::<WINDOW>(seq)];
			slot.data[..payload.len()].copy_from_slice(payload);
			slot.len = payload.len();
			slot.filled = true;
			return;
		}
		deliver(payload);
		self.recv_next = self.recv_next.wrapping_add(1);
		loop {
			let slot = &mut self.recv_slots[slot_index::<WINDOW>(self.recv_next)];
			if !slot.filled {
				break;
			}
			slot.filled = false;
			deliver(&slot.data[..slot.len]);
			self.recv_next = self.recv_next.wrapping_add(1);
		}
	}
}

fn slot_index<const WINDOW: usize>(seq: u16) -> usize {
	usize::from(seq) % WINDOW
}

fn retransmit_delay(config: &Config, previous_transmissions: u8) -> Duration {
	let doubled = config
		.retransmit_timeout
		.checked_mul(1 << previous_transmissions.min(16))
		.unwrap_or(Duration::MAX);
	doubled.min(config.max_retransmit_timeout)
}
