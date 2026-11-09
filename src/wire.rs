//! Frame layout. All integers are little-endian.
//!
//! ```text
//! HELLO: kind src dst
//! DATA:  kind src dst seq ack sack payload
//! ACK:   kind src dst ack sack
//! ```
//!
//! `src` and `dst` are 4-byte session ids (`dst` is 0 when unknown), `seq` and
//! `ack` are 2 bytes and `sack` is 4. `ack` is the first sequence number the
//! sender has not received, and bit `i` of `sack` means it has received
//! `ack + 1 + i`.

use core::num::NonZeroU32;

const HELLO: u8 = 1;
const DATA: u8 = 2;
const ACK: u8 = 3;

const SESSIONS_LEN: usize = 9;
const ACK_INFO_LEN: usize = 6;

/// Bytes a data frame adds in front of its payload.
pub const DATA_HEADER_LEN: usize = SESSIONS_LEN + 2 + ACK_INFO_LEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AckInfo {
	pub ack: u16,
	pub sack: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Body<'a> {
	Hello,
	Data {
		seq: u16,
		ack: AckInfo,
		payload: &'a [u8],
	},
	Ack(AckInfo),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame<'a> {
	pub src: NonZeroU32,
	pub dst: Option<NonZeroU32>,
	pub body: Body<'a>,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
	u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
	u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn ack_info_at(bytes: &[u8], at: usize) -> AckInfo {
	AckInfo {
		ack: u16_at(bytes, at),
		sack: u32_at(bytes, at + 2),
	}
}

impl<'a> Frame<'a> {
	/// Returns `None` for anything that is not a well-formed frame.
	pub fn decode(bytes: &'a [u8]) -> Option<Self> {
		if bytes.len() < SESSIONS_LEN {
			return None;
		}
		let src = NonZeroU32::new(u32_at(bytes, 1))?;
		let dst = NonZeroU32::new(u32_at(bytes, 5));
		let rest = &bytes[SESSIONS_LEN..];
		let body = match bytes[0] {
			HELLO if rest.is_empty() => Body::Hello,
			DATA if rest.len() >= 2 + ACK_INFO_LEN => Body::Data {
				seq: u16_at(rest, 0),
				ack: ack_info_at(rest, 2),
				payload: &rest[2 + ACK_INFO_LEN..],
			},
			ACK if rest.len() == ACK_INFO_LEN => Body::Ack(ack_info_at(rest, 0)),
			_ => return None,
		};
		Some(Frame { src, dst, body })
	}

	/// Writes the frame to the start of `out` and returns its length.
	/// Panics if `out` is too small.
	pub fn encode(&self, out: &mut [u8]) -> usize {
		let kind = match self.body {
			Body::Hello => HELLO,
			Body::Data { .. } => DATA,
			Body::Ack(_) => ACK,
		};
		out[0] = kind;
		out[1..5].copy_from_slice(&self.src.get().to_le_bytes());
		out[5..9].copy_from_slice(&self.dst.map_or(0, NonZeroU32::get).to_le_bytes());
		let rest = &mut out[SESSIONS_LEN..];
		let body_len = match self.body {
			Body::Hello => 0,
			Body::Data { seq, ack, payload } => {
				rest[..2].copy_from_slice(&seq.to_le_bytes());
				write_ack_info(&mut rest[2..], ack);
				let start = 2 + ACK_INFO_LEN;
				rest[start..start + payload.len()].copy_from_slice(payload);
				start + payload.len()
			}
			Body::Ack(ack) => {
				write_ack_info(rest, ack);
				ACK_INFO_LEN
			}
		};
		SESSIONS_LEN + body_len
	}
}

fn write_ack_info(out: &mut [u8], info: AckInfo) {
	out[..2].copy_from_slice(&info.ack.to_le_bytes());
	out[2..6].copy_from_slice(&info.sack.to_le_bytes());
}

#[cfg(test)]
mod tests {
	use super::*;
	use proptest::prelude::*;

	fn session() -> impl Strategy<Value = NonZeroU32> {
		(1..=u32::MAX).prop_map(|n| NonZeroU32::new(n).unwrap())
	}

	proptest! {
		#[test]
		fn frames_round_trip(
			src in session(),
			dst in proptest::option::of(session()),
			kind in 0..3u8,
			seq: u16,
			ack: u16,
			sack: u32,
			payload in proptest::collection::vec(any::<u8>(), 0..64),
		) {
			let ack = AckInfo { ack, sack };
			let body = match kind {
				0 => Body::Hello,
				1 => Body::Data { seq, ack, payload: &payload },
				_ => Body::Ack(ack),
			};
			let frame = Frame { src, dst, body };
			let mut buf = [0u8; 128];
			let len = frame.encode(&mut buf);
			prop_assert_eq!(Frame::decode(&buf[..len]), Some(frame));
		}

		#[test]
		fn decoding_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
			let _ = Frame::decode(&bytes);
		}
	}
}
