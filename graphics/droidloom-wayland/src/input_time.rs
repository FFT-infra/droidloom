//! Wayland input event times on the `CLOCK_MONOTONIC` Android shares.
//!
//! Denial stamps `wl_touch`, `wl_pointer` and `wl_keyboard` events with the
//! libinput time in milliseconds, truncated to 32 bits. Through
//! `zwp_input_timestamps_v1` it also sends the full libinput time just before
//! each event. The cell has no time namespace, so that clock is Android's
//! `CLOCK_MONOTONIC` too.

use std::collections::VecDeque;

use wayland_client::{
    Connection, Dispatch, QueueHandle, protocol::wl_keyboard, protocol::wl_pointer,
    protocol::wl_touch,
};
use wayland_protocols::wp::input_timestamps::zv1::client::{
    zwp_input_timestamps_manager_v1::ZwpInputTimestampsManagerV1,
    zwp_input_timestamps_v1::{self, ZwpInputTimestampsV1},
};

use crate::App;

/// A compositor time further than this from receipt belongs to another clock.
const FOREIGN_CLOCK_MILLIS: u64 = 1_000;

/// Full times waiting for their event. SCTK holds touch events until
/// `wl_touch.frame`, so one frame's times queue up before its handlers run.
const PRECISE_QUEUE: usize = 64;

/// Event times of one input stream, never moving backwards.
#[derive(Default)]
pub(super) struct Stream {
    last: u64,
}

impl Stream {
    /// Nanoseconds for an event with the compositor's `millis` and, when the
    /// compositor sent one, its full time `precise`, received at `now`.
    pub fn event(&mut self, millis: u32, precise: Option<u64>, now: u64) -> u64 {
        let time = precise
            .map(|time| time.min(now))
            .or_else(|| unwrap_millis(millis, now))
            .unwrap_or(now);
        self.at(time)
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

/// A seat device whose events can carry full times.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Device {
    Keyboard,
    Pointer,
    Touch,
}

/// `zwp_input_timestamps_v1` subscriptions and the times they delivered.
#[derive(Default)]
pub(super) struct Timestamps {
    manager: Option<ZwpInputTimestampsManagerV1>,
    keyboard: Subscription,
    pointer: Subscription,
    touch: Subscription,
}

#[derive(Default)]
struct Subscription {
    object: Option<ZwpInputTimestampsV1>,
    precise: Precise,
}

impl Timestamps {
    pub fn new(manager: Option<ZwpInputTimestampsManagerV1>) -> Self {
        Self {
            manager,
            ..Self::default()
        }
    }

    pub fn keyboard(&mut self, keyboard: &wl_keyboard::WlKeyboard, qh: &QueueHandle<App>) {
        let object = self
            .manager
            .as_ref()
            .map(|manager| manager.get_keyboard_timestamps(keyboard, qh, Device::Keyboard));
        self.keyboard.replace(object);
    }

    pub fn pointer(&mut self, pointer: &wl_pointer::WlPointer, qh: &QueueHandle<App>) {
        let object = self
            .manager
            .as_ref()
            .map(|manager| manager.get_pointer_timestamps(pointer, qh, Device::Pointer));
        self.pointer.replace(object);
    }

    pub fn touch(&mut self, touch: &wl_touch::WlTouch, qh: &QueueHandle<App>) {
        let object = self
            .manager
            .as_ref()
            .map(|manager| manager.get_touch_timestamps(touch, qh, Device::Touch));
        self.touch.replace(object);
    }

    /// Stop following a device the seat removed.
    pub fn remove(&mut self, device: Device) {
        self.subscription(device).replace(None);
    }

    /// The full time of `device`'s next event stamped `millis`, if Denial sent one.
    pub fn take(&mut self, device: Device, millis: u32) -> Option<u64> {
        self.subscription(device).precise.take(millis)
    }

    fn subscription(&mut self, device: Device) -> &mut Subscription {
        match device {
            Device::Keyboard => &mut self.keyboard,
            Device::Pointer => &mut self.pointer,
            Device::Touch => &mut self.touch,
        }
    }
}

impl Subscription {
    fn replace(&mut self, object: Option<ZwpInputTimestampsV1>) {
        if let Some(old) = std::mem::replace(&mut self.object, object) {
            old.destroy();
        }
        self.precise = Precise::default();
    }
}

/// Full times in arrival order, each for the next event of its device.
#[derive(Default)]
struct Precise(VecDeque<u64>);

impl Precise {
    fn push(&mut self, nanos: u64) {
        if self.0.len() == PRECISE_QUEUE {
            self.0.pop_front();
        }
        self.0.push_back(nanos);
    }

    /// The oldest time whose milliseconds are `millis`. Older times belong
    /// to events the presenter ignored and are dropped.
    fn take(&mut self, millis: u32) -> Option<u64> {
        while let Some(nanos) = self.0.pop_front() {
            #[allow(clippy::cast_possible_truncation)]
            if (nanos / 1_000_000) as u32 == millis {
                return Some(nanos);
            }
        }
        None
    }
}

impl Dispatch<ZwpInputTimestampsManagerV1, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpInputTimestampsManagerV1,
        _event: <ZwpInputTimestampsManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpInputTimestampsV1, Device> for App {
    fn event(
        state: &mut Self,
        _proxy: &ZwpInputTimestampsV1,
        event: zwp_input_timestamps_v1::Event,
        device: &Device,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let zwp_input_timestamps_v1::Event::Timestamp {
            tv_sec_hi,
            tv_sec_lo,
            tv_nsec,
        } = event
        else {
            return;
        };
        let seconds = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
        if let Some(nanos) = seconds
            .checked_mul(1_000_000_000)
            .and_then(|nanos| nanos.checked_add(u64::from(tv_nsec)))
        {
            state
                .input_timestamps
                .subscription(*device)
                .precise
                .push(nanos);
        }
    }
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
        assert_eq!(stream.event(1, None, now), now);
    }

    #[test]
    fn each_stream_is_non_decreasing() {
        let mut stream = Stream::default();
        assert_eq!(stream.event(100, None, 101 * MS), 100 * MS);
        assert_eq!(stream.received(102 * MS), 102 * MS);
        // A later callback for an earlier hardware time keeps the order Android saw.
        assert_eq!(stream.event(101, None, 103 * MS), 102 * MS);
        assert_eq!(stream.event(103, None, 103 * MS), 103 * MS);
        let mut other = Stream::default();
        assert_eq!(other.event(101, None, 103 * MS), 101 * MS);
    }

    #[test]
    fn full_times_replace_milliseconds() {
        let mut stream = Stream::default();
        let precise = 100 * MS + 386_118;
        assert_eq!(stream.event(100, Some(precise), 101 * MS), precise);
        // Never later than receipt.
        assert_eq!(
            stream.event(102, Some(103 * MS), 102 * MS + 5),
            102 * MS + 5
        );
    }

    #[test]
    fn full_times_pair_with_their_events_in_order() {
        let mut precise = Precise::default();
        for nanos in [100 * MS + 1, 108 * MS + 2, 116 * MS + 3] {
            precise.push(nanos);
        }
        assert_eq!(precise.take(100), Some(100 * MS + 1));
        // The 108 ms event was ignored; its time is dropped on the way.
        assert_eq!(precise.take(116), Some(116 * MS + 3));
        assert_eq!(precise.take(124), None);
    }
}
