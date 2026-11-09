//! Runs two links over a simulated radio that loses, duplicates and reorders
//! frames, and checks what each application receives.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::time::Duration;

use espnow_reliable::{Config, Event, Instant, Link};
use proptest::prelude::*;

const WINDOW: usize = 8;
const FRAME: usize = 48;
const PAYLOAD: usize = FRAME - espnow_reliable::DATA_HEADER_LEN;
const LATENCY_MICROS: u64 = 1_000;

type TestLink = Link<WINDOW, FRAME>;

struct Rng(u64);

impl Rng {
	fn next(&mut self) -> u64 {
		self.0 ^= self.0 << 13;
		self.0 ^= self.0 >> 7;
		self.0 ^= self.0 << 17;
		self.0
	}

	fn chance(&mut self, probability: f64) -> bool {
		((self.next() >> 11) as f64 / (1u64 << 53) as f64) < probability
	}

	fn below(&mut self, bound: u64) -> u64 {
		if bound == 0 { 0 } else { self.next() % bound }
	}

	fn session(&mut self) -> NonZeroU32 {
		NonZeroU32::new(self.next() as u32 | 1).unwrap()
	}
}

#[derive(Clone, Debug)]
struct Radio {
	loss: f64,
	duplication: f64,
	jitter_micros: u64,
}

struct Peer {
	link: Box<TestLink>,
	outbox: Vec<Vec<u8>>,
	cursor: usize,
	received: Vec<Vec<u8>>,
	resets: usize,
	failed: bool,
}

impl Peer {
	fn new(session: NonZeroU32, config: Config, outbox: Vec<Vec<u8>>) -> Self {
		Peer {
			link: Box::new(TestLink::new(session, config)),
			outbox,
			cursor: 0,
			received: Vec::new(),
			resets: 0,
			failed: false,
		}
	}

	fn handle_events(&mut self) {
		while let Some(event) = self.link.poll_event() {
			match event {
				Event::Connected => {}
				Event::Reset => {
					self.resets += 1;
					self.cursor = 0;
					self.received.clear();
				}
				Event::Failed => self.failed = true,
			}
		}
	}

	fn is_done(&self, other: &Peer) -> bool {
		self.cursor == self.outbox.len() && self.link.is_idle() && other.received == self.outbox
	}
}

struct Simulation {
	rng: Rng,
	radio: Radio,
	now: Instant,
	/// Frames in the air, keyed by arrival time and a tiebreaker.
	air: BTreeMap<(Instant, u64), (usize, Vec<u8>)>,
	peers: [Peer; 2],
}

impl Simulation {
	fn new(seed: u64, radio: Radio, config: Config, outboxes: [Vec<Vec<u8>>; 2]) -> Self {
		let mut rng = Rng(seed | 1);
		let [first, second] = outboxes;
		let peers = [
			Peer::new(rng.session(), config, first),
			Peer::new(rng.session(), config, second),
		];
		Simulation {
			rng,
			radio,
			now: Instant::ZERO,
			air: BTreeMap::new(),
			peers,
		}
	}

	fn restart(&mut self, side: usize, config: Config) {
		let session = self.rng.session();
		let outbox = std::mem::take(&mut self.peers[side].outbox);
		self.peers[side] = Peer::new(session, config, outbox);
	}

	fn transmit(&mut self, from: usize) {
		let mut buf = [0u8; FRAME];
		let peer = &mut self.peers[from];
		while peer.cursor < peer.outbox.len() && peer.link.send(&peer.outbox[peer.cursor]).is_ok() {
			peer.cursor += 1;
		}
		while let Some(len) = self.peers[from].link.poll_transmit(self.now, &mut buf) {
			let copies = if self.rng.chance(self.radio.duplication) {
				2
			} else {
				1
			};
			for _ in 0..copies {
				if self.rng.chance(self.radio.loss) {
					continue;
				}
				let delay = LATENCY_MICROS + self.rng.below(self.radio.jitter_micros);
				let arrival = self.now + Duration::from_micros(delay);
				let tiebreak = self.rng.next();
				self.air
					.insert((arrival, tiebreak), (1 - from, buf[..len].to_vec()));
			}
		}
		self.peers[from].handle_events();
	}

	fn deliver_due(&mut self) {
		while let Some(entry) = self.air.first_entry() {
			if entry.key().0 > self.now {
				break;
			}
			let (to, frame) = entry.remove();
			let peer = &mut self.peers[to];
			let received = &mut peer.received;
			peer.link
				.handle(&frame, self.now, |message| received.push(message.to_vec()));
			peer.handle_events();
		}
	}

	fn next_wakeup(&self) -> Option<Instant> {
		let timers = self
			.peers
			.iter()
			.filter_map(|peer| peer.link.poll_timeout());
		let arrivals = self.air.keys().next().map(|key| key.0);
		timers.chain(arrivals).min().map(|at| at.max(self.now))
	}

	fn is_done(&self) -> bool {
		self.peers[0].is_done(&self.peers[1]) && self.peers[1].is_done(&self.peers[0])
	}

	fn any_failed(&self) -> bool {
		self.peers.iter().any(|peer| peer.failed)
	}

	/// Runs until both streams are delivered, a link fails, nothing is left
	/// to do, or `until` passes.
	fn run(&mut self, until: Instant) {
		while self.now <= until {
			self.deliver_due();
			self.transmit(0);
			self.transmit(1);
			if self.is_done() || self.any_failed() {
				return;
			}
			match self.next_wakeup() {
				Some(next) => self.now = next,
				None => return,
			}
		}
	}
}

fn patient_config() -> Config {
	Config {
		max_retransmits: 40,
		..Config::default()
	}
}

fn messages() -> impl Strategy<Value = Vec<Vec<u8>>> {
	prop::collection::vec(prop::collection::vec(any::<u8>(), 0..=PAYLOAD), 0..40)
}

fn unreliable_radio() -> impl Strategy<Value = Radio> {
	(0.0..0.3f64, 0.0..0.2f64, 0..20_000u64).prop_map(|(loss, duplication, jitter_micros)| Radio {
		loss,
		duplication,
		jitter_micros,
	})
}

fn assert_prefix(received: &[Vec<u8>], sent: &[Vec<u8>]) -> Result<(), TestCaseError> {
	prop_assert!(received.len() <= sent.len(), "received more than was sent");
	prop_assert_eq!(received, &sent[..received.len()]);
	Ok(())
}

const HOUR: Instant = Instant::from_millis(3_600_000);

proptest! {
	#[test]
	fn delivers_everything_in_order(
		seed: u64,
		radio in unreliable_radio(),
		first in messages(),
		second in messages(),
	) {
		let mut sim = Simulation::new(seed, radio, patient_config(), [first.clone(), second.clone()]);
		sim.run(HOUR);
		prop_assert!(!sim.any_failed());
		prop_assert_eq!(sim.peers.iter().map(|peer| peer.resets).sum::<usize>(), 0);
		prop_assert_eq!(&sim.peers[1].received, &first);
		prop_assert_eq!(&sim.peers[0].received, &second);
	}

	#[test]
	fn never_duplicates_or_reorders(
		seed: u64,
		loss in 0.0..=1.0f64,
		duplication in 0.0..=1.0f64,
		jitter_micros in 0..200_000u64,
		first in messages(),
		second in messages(),
	) {
		let radio = Radio { loss, duplication, jitter_micros };
		let mut sim = Simulation::new(seed, radio, Config::default(), [first.clone(), second.clone()]);
		sim.run(HOUR);
		assert_prefix(&sim.peers[1].received, &first)?;
		assert_prefix(&sim.peers[0].received, &second)?;
	}

	#[test]
	fn recovers_from_restarts(
		seed: u64,
		radio in unreliable_radio(),
		restarts in prop::collection::vec((0..2usize, 1..2_000u64), 1..4),
		first in messages(),
		second in messages(),
	) {
		let config = patient_config();
		let mut sim = Simulation::new(seed, radio, config, [first.clone(), second.clone()]);
		for (side, after_millis) in restarts {
			let until = sim.now + Duration::from_millis(after_millis);
			sim.run(until);
			prop_assert!(!sim.any_failed());
			sim.now = sim.now.max(until);
			sim.restart(side, config);
		}
		sim.run(sim.now + Duration::from_secs(3_600));
		prop_assert!(!sim.any_failed());
		prop_assert_eq!(&sim.peers[1].received, &first);
		prop_assert_eq!(&sim.peers[0].received, &second);
	}
}

#[test]
fn sequence_numbers_wrap() {
	let radio = Radio {
		loss: 0.05,
		duplication: 0.05,
		jitter_micros: 3_000,
	};
	let messages: Vec<Vec<u8>> = (0..70_000u32).map(|n| n.to_le_bytes().to_vec()).collect();
	let mut sim = Simulation::new(
		7,
		radio,
		patient_config(),
		[messages.clone(), messages.clone()],
	);
	sim.run(Instant::from_millis(u64::MAX / 1000));
	assert!(!sim.any_failed());
	assert_eq!(sim.peers[1].received, messages);
	assert_eq!(sim.peers[0].received, messages);
}

#[test]
fn a_restarted_peer_is_reported_once() {
	let radio = Radio {
		loss: 0.0,
		duplication: 0.0,
		jitter_micros: 0,
	};
	let messages = vec![vec![1, 2, 3]];
	let mut sim = Simulation::new(
		1,
		radio,
		Config::default(),
		[messages.clone(), messages.clone()],
	);
	sim.run(HOUR);
	assert!(sim.is_done());
	sim.restart(1, Config::default());
	sim.run(sim.now + Duration::from_secs(60));
	assert!(sim.is_done());
	assert_eq!(sim.peers[0].resets, 1);
	assert_eq!(sim.peers[0].received, messages);
}

#[test]
fn an_unreachable_peer_fails_the_link() {
	let radio = Radio {
		loss: 0.0,
		duplication: 0.0,
		jitter_micros: 0,
	};
	let mut sim = Simulation::new(1, radio, Config::default(), [vec![vec![1]], Vec::new()]);
	sim.run(HOUR);
	assert!(sim.is_done());
	sim.radio.loss = 1.0;
	sim.peers[0].outbox.push(vec![2]);
	sim.run(sim.now + Duration::from_secs(60));
	assert!(sim.peers[0].failed);
	assert!(sim.peers[0].link.send(&[3]).is_err());
}

/// Passes frames between two links with no loss until neither has anything
/// to send, and returns every frame `b` sent.
fn exchange(a: &mut TestLink, b: &mut TestLink, now: Instant) -> Vec<Vec<u8>> {
	let mut buf = [0u8; FRAME];
	let mut sent_by_b = Vec::new();
	loop {
		let mut quiet = true;
		while let Some(len) = a.poll_transmit(now, &mut buf) {
			b.handle(&buf[..len], now, |_| {});
			quiet = false;
		}
		while let Some(len) = b.poll_transmit(now, &mut buf) {
			sent_by_b.push(buf[..len].to_vec());
			a.handle(&buf[..len], now, |_| {});
			quiet = false;
		}
		if quiet {
			return sent_by_b;
		}
	}
}

#[test]
fn late_frames_from_a_restarted_peer_are_ignored() {
	let session = |n| NonZeroU32::new(n).unwrap();
	let config = Config::default();
	let mut a = Box::new(TestLink::new(session(1), config));
	let mut old = Box::new(TestLink::new(session(2), config));
	let start = Instant::from_millis(1_000);
	let old_frames = exchange(&mut a, &mut old, start);
	assert_eq!(a.poll_event(), Some(Event::Connected));

	let mut new = Box::new(TestLink::new(session(3), config));
	let restarted = start + config.max_frame_age;
	exchange(&mut a, &mut new, restarted);
	assert_eq!(a.poll_event(), Some(Event::Reset));
	assert_eq!(a.poll_event(), Some(Event::Connected));

	let late = restarted + config.max_frame_age / 2;
	for frame in &old_frames {
		a.handle(frame, late, |_| {
			panic!("delivered a message from the old session")
		});
	}
	assert_eq!(a.poll_event(), None);
	assert_eq!(a.peer_session(), Some(session(3)));
	assert!(a.is_connected());
}
