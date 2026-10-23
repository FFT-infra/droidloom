//! Wayland input event times on the `CLOCK_MONOTONIC` Android shares.
//!
//! Denial stamps `wl_touch`, `wl_pointer` and `wl_keyboard` events with the
//! libinput time in milliseconds, truncated to 32 bits. The cell has no time
//! namespace, so that clock is Android's `CLOCK_MONOTONIC` too.

/// A compositor time further than this from receipt belongs to another clock.
const FOREIGN_CLOCK_MILLIS: u64 = 1_000;

/// Event times of one input stream, never moving backwards.
#[derive(Default)]
pub(super) struct Stream {
    last: u64,
}

impl Stream {
    /// Nanoseconds for a millisecond event time received at `now`.
    pub fn event(&mut self, millis: u32, now: u64) -> u64 {
        self.at(unwrap_millis(millis, now).unwrap_or(now))
    }

    /// Nanoseconds for an event without a compositor time, received at `now`.
    pub fn received(&mut self, now: u64) -> u64 {
        self.at(now)
    }

    fn at(&mut self, time: u64) -> u64 {
        self.last = self.last.max(time);
        self.last
    }
}

/// The time nearest `now` whose milliseconds match `millis` modulo 2^32, or
/// `None` when it is too far from `now` to be on the same clock.
fn unwrap_millis(millis: u32, now: u64) -> Option<u64> {
    let now_millis = now / 1_000_000;
    // The compositor sent the low 32 bits; the signed difference picks the
    // nearest wrap on either side of now.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let offset = i64::from(millis.wrapping_sub(now_millis as u32) as i32);
    if offset.unsigned_abs() > FOREIGN_CLOCK_MILLIS {
        return None;
    }
    let time = now_millis
        .checked_add_signed(offset)?
        .checked_mul(1_000_000)?;
    Some(time.min(now))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn millisecond_times_extend_to_nanoseconds_before_receipt() {
        let now = 5_000 * MS + 700_000;
        assert_eq!(unwrap_millis(4_996, now), Some(4_996 * MS));
        assert_eq!(unwrap_millis(5_000, now), Some(5_000 * MS));
        // A compositor rounding ahead of receipt never yields a future time.
        assert_eq!(unwrap_millis(5_001, now), Some(now));
    }

    #[test]
    fn times_unwrap_across_the_32_bit_millisecond_boundary() {
        let wrap = 1_u64 << 32;
        let after = (wrap + 3) * MS;
        assert_eq!(unwrap_millis(u32::MAX - 1, after), Some((wrap - 2) * MS));
        assert_eq!(unwrap_millis(2, after), Some((wrap + 2) * MS));
        let before = (wrap - 2) * MS;
        assert_eq!(unwrap_millis(u32::MAX - 5, before), Some((wrap - 6) * MS));
    }

    #[test]
    fn foreign_clocks_and_early_boot_fall_back_to_receipt() {
        let now = 90_000 * MS;
        assert_eq!(unwrap_millis(88_999, now), None);
        assert_eq!(unwrap_millis(89_000, now), Some(89_000 * MS));
        assert_eq!(unwrap_millis(u32::MAX, 500 * MS), None);
        let mut stream = Stream::default();
        assert_eq!(stream.event(1, now), now);
    }

    #[test]
    fn each_stream_is_non_decreasing() {
        let mut stream = Stream::default();
        assert_eq!(stream.event(100, 101 * MS), 100 * MS);
        assert_eq!(stream.received(102 * MS), 102 * MS);
        // A later callback for an earlier hardware time keeps the order Android saw.
        assert_eq!(stream.event(101, 103 * MS), 102 * MS);
        assert_eq!(stream.event(103, 103 * MS), 103 * MS);
        let mut other = Stream::default();
        assert_eq!(other.event(101, 103 * MS), 101 * MS);
    }
}
