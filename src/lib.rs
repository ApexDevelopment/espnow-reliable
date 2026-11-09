//! Reliable, ordered message delivery between two ESP-NOW peers.
//!
//! ESP-NOW frames can be lost, and a radio that resends a frame after a missed
//! acknowledgement can deliver it twice. A [`Link`] numbers each message, resends
//! it until the peer acknowledges it, and hands received messages to the
//! application exactly once and in order. It also notices when the peer
//! restarts, so the application can start over instead of mixing old and new
//! state.
//!
//! The crate does no I/O and keeps no clock. The application sends and
//! receives datagrams with whatever ESP-NOW driver it uses, keeps one `Link`
//! per peer MAC address, and drives it:
//!
//! 1. Queue messages with [`Link::send`].
//! 2. Pass every datagram from the peer to [`Link::handle`].
//! 3. Send whatever [`Link::poll_transmit`] returns until it returns `None`,
//!    after each of the calls above and whenever [`Link::poll_timeout`] passes.
//! 4. Check [`Link::poll_event`] for connection changes.
//!
//! If the driver reports frames the peer did not acknowledge, as ESP-NOW
//! does, pass them to [`Link::frame_lost`] so they are resent sooner.
//!
//! Both peers must use this crate. Broadcast is not covered: discovery is up
//! to the application.
//!
//! ```
//! use core::num::NonZeroU32;
//! use espnow_reliable::{Config, Event, Instant, Link, MAX_DATA_LEN_V2};
//!
//! // Session ids must come from a random number generator.
//! let session = |n| NonZeroU32::new(n).unwrap();
//! let mut alice: Box<Link> = Box::new(Link::new(session(0x1234), Config::default()));
//! let mut bob: Box<Link> = Box::new(Link::new(session(0x5678), Config::default()));
//!
//! alice.send(b"hello").unwrap();
//!
//! // Stand-in for the radio: pass frames back and forth until both are quiet.
//! let now = Instant::from_millis(0);
//! let mut frame = [0u8; MAX_DATA_LEN_V2];
//! let mut received = Vec::new();
//! loop {
//! 	let mut quiet = true;
//! 	while let Some(len) = alice.poll_transmit(now, &mut frame) {
//! 		bob.handle(&frame[..len], now, |message| received.push(message.to_vec()));
//! 		quiet = false;
//! 	}
//! 	while let Some(len) = bob.poll_transmit(now, &mut frame) {
//! 		alice.handle(&frame[..len], now, |_| {});
//! 		quiet = false;
//! 	}
//! 	if quiet {
//! 		break;
//! 	}
//! }
//!
//! assert_eq!(received, [b"hello"]);
//! assert_eq!(bob.poll_event(), Some(Event::Connected));
//! assert!(alice.is_idle());
//! ```
#![no_std]
// The examples are indented with tabs, like the code.
#![allow(clippy::tabs_in_doc_comments)]

mod link;
mod time;
mod wire;

pub use link::{Config, Event, Link, SendError};
pub use time::Instant;
pub use wire::DATA_HEADER_LEN;

/// The largest ESP-NOW v1 payload.
pub const MAX_DATA_LEN_V1: usize = 250;

/// The largest ESP-NOW v2 payload. Peers running v1 drop longer frames.
pub const MAX_DATA_LEN_V2: usize = 1470;
