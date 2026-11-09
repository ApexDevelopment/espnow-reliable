use core::ops::Add;
use core::time::Duration;

/// A point in time, counted in microseconds from any fixed starting point,
/// such as boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(u64);

impl Instant {
	pub const ZERO: Instant = Instant(0);

	pub const fn from_micros(micros: u64) -> Self {
		Instant(micros)
	}

	pub const fn from_millis(millis: u64) -> Self {
		Instant(millis.saturating_mul(1000))
	}

	pub const fn as_micros(self) -> u64 {
		self.0
	}

	/// How long after `earlier` this is, or zero if it is not after it.
	pub fn saturating_duration_since(self, earlier: Instant) -> Duration {
		Duration::from_micros(self.0.saturating_sub(earlier.0))
	}
}

impl Add<Duration> for Instant {
	type Output = Instant;

	fn add(self, duration: Duration) -> Instant {
		let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
		Instant(self.0.saturating_add(micros))
	}
}
