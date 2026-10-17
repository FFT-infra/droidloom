//! Latest window geometry awaiting application after a Wayland dispatch batch.

use std::time::{Duration, Instant};

use droidloom_window_policy::LogicalSize;

/// Shortest interval between two relayouts of one live resize grab.
///
/// A compositor previews a live resize by scaling the last committed buffer to
/// the pointer, so intermediate geometry needs no work from this side: applying
/// it would retire a three-buffer pool, allocate another, and drive a full
/// Android task relayout for an image that is about to be replaced.
///
/// Waiting for the whole grab is not an option either. Denial paces its
/// configures on the client's commit, so a window that never commits stops
/// receiving sizes and stays rendered as its last buffer stretched onto the
/// last target. Committing on this interval keeps that loop alive while
/// bounding a drag to a few relayouts instead of one per pointer sample.
///
/// The interval is armed once per burst of samples, so later samples replace
/// the queued geometry without pushing the commit back. The grab's terminating
/// configure applies the real final size immediately, and the same interval
/// covers a compositor that never sends one.
const RESIZE_SETTLE: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Configure {
    pub size: LogicalSize,
    pub scale_120: u32,
    pub remember: bool,
}

impl Configure {
    pub fn changes_geometry(self, size: Option<(u32, u32)>, scale_120: u32) -> bool {
        size != Some((self.size.width, self.size.height)) || scale_120 != self.scale_120
    }
}

pub struct Pending {
    size: Option<LogicalSize>,
    scale_120: u32,
    remember: bool,
    /// The compositor owns a live interactive resize of this window.
    interactive: bool,
    /// Earliest instant at which the queued geometry may be applied.
    apply_after: Option<Instant>,
}

impl Pending {
    pub fn new(scale_120: u32) -> Self {
        Self {
            size: None,
            scale_120,
            remember: false,
            interactive: false,
            apply_after: None,
        }
    }

    // A zero XDG axis inherits the latest requested size, even before that
    // size has been applied. The caller resolves policy only for the first size.
    pub fn logical_size(&self, applied: Option<(u32, u32)>) -> Option<LogicalSize> {
        self.size
            .or_else(|| applied.map(|(width, height)| LogicalSize { width, height }))
    }

    pub fn configure(&mut self, size: LogicalSize, remember: bool, interactive: bool) {
        self.configure_at(size, remember, interactive, Instant::now());
    }

    /// Queue geometry against an explicit clock.
    ///
    /// `interactive` marks a live resize grab. A grab's intermediate geometry
    /// waits for [`RESIZE_SETTLE`]; the compositor's terminating configure
    /// applies the real size and the interval applies it if that configure
    /// never arrives.
    pub fn configure_at(
        &mut self,
        size: LogicalSize,
        remember: bool,
        interactive: bool,
        now: Instant,
    ) {
        if self.size == Some(size) && self.interactive == interactive {
            // Repeating one geometry must not push its own deadline forward,
            // or a chattering grab could postpone the relayout indefinitely.
            self.remember = remember;
            return;
        }
        self.size = Some(size);
        self.remember = remember;
        self.interactive = interactive;
        if !interactive {
            self.apply_after = Some(now);
        } else if self.apply_after.is_none() {
            // Arm once per burst: restarting the interval on every sample would
            // starve the commit for the length of the drag.
            self.apply_after = Some(now + RESIZE_SETTLE);
        }
    }

    /// Whether the compositor still owns an interactive resize of this window.
    #[cfg(test)]
    pub fn is_resizing(&self) -> bool {
        self.interactive
    }

    /// Whether the queued geometry may be applied at `now`.
    pub fn ready(&self, now: Instant) -> bool {
        self.size.is_some() && self.apply_after.is_none_or(|after| now >= after)
    }

    /// Wake-up deadline of geometry that is still settling.
    pub fn deadline(&self, now: Instant) -> Option<Instant> {
        self.size.and(self.apply_after).filter(|after| *after > now)
    }

    pub fn preferred_scale(&mut self, scale_120: u32, applied: Option<(u32, u32)>) -> bool {
        if self.scale_120 == scale_120 {
            return false;
        }
        self.scale_120 = scale_120;
        // Scale alone must not allocate a provisional pool before the initial
        // XDG configure. It must also preserve a newer queued logical size.
        self.size = self.logical_size(applied);
        if self.size.is_some() {
            // A scale change is a discrete output event, not a resize sample.
            // Never leave it waiting behind a grab that is still settling.
            self.apply_after = Some(Instant::now());
        }
        self.size.is_some()
    }

    pub fn take(&mut self) -> Option<Configure> {
        let size = self.size.take()?;
        self.apply_after = None;
        self.interactive = false;
        let remember = std::mem::take(&mut self.remember);
        Some(Configure {
            size,
            scale_120: self.scale_120,
            remember,
        })
    }

    pub fn cancel(&mut self) {
        self.size = None;
        self.remember = false;
        self.interactive = false;
        self.apply_after = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readiness::{RESIZE, Tasks, UNMAP};
    use droidloom_denial_protocol::TaskObjectId;

    fn size(width: u32, height: u32) -> LogicalSize {
        LogicalSize { width, height }
    }

    #[test]
    fn burst_replaces_intermediate_sizes_and_keeps_windows_independent() {
        let mut work = Tasks::default();
        let first = TaskObjectId(1);
        let second = TaskObjectId(2);
        let mut windows = [(first, Pending::new(120)), (second, Pending::new(120))];
        for width in 800..1000 {
            windows[0].1.configure(size(width, 600), false, false);
            work.mark(first, RESIZE);
        }
        windows[1].1.configure(size(640, 480), true, false);
        work.mark(second, RESIZE);
        let mut applied = Vec::new();
        while let Some(object) = work.pop(RESIZE) {
            let pending = &mut windows.iter_mut().find(|(id, _)| *id == object).unwrap().1;
            applied.push((object, pending.take().unwrap()));
        }
        assert_eq!(
            applied,
            [
                (
                    first,
                    Configure {
                        size: size(999, 600),
                        scale_120: 120,
                        remember: false
                    }
                ),
                (
                    second,
                    Configure {
                        size: size(640, 480),
                        scale_120: 120,
                        remember: true
                    }
                ),
            ]
        );
        assert!(!work.pending());
    }

    #[test]
    fn unspecified_axes_inherit_pending_size_before_applied_size() {
        let mut pending = Pending::new(120);
        let applied = Some((800, 600));
        pending.configure(size(1000, 700), false, false);
        let previous = pending.logical_size(applied).unwrap();
        pending.configure(size(previous.width, 750), false, false);
        let previous = pending.logical_size(applied).unwrap();
        pending.configure(size(1100, previous.height), true, false);
        assert_eq!(pending.take().unwrap().size, size(1100, 750));
        assert!(pending.take().is_none());
    }

    #[test]
    fn scale_and_initial_configure_order_produce_one_final_geometry() {
        for scale_first in [false, true] {
            let mut pending = Pending::new(120);
            if scale_first {
                assert!(!pending.preferred_scale(150, None));
                assert!(pending.take().is_none());
            }
            pending.configure(size(900, 700), false, false);
            if !scale_first {
                assert!(pending.preferred_scale(150, None));
            }
            assert_eq!(
                pending.take(),
                Some(Configure {
                    size: size(900, 700),
                    scale_120: 150,
                    remember: false,
                })
            );
        }
    }

    #[test]
    fn scale_updates_preserve_pending_size_and_scale_only_uses_applied_size() {
        let mut pending = Pending::new(120);
        pending.configure(size(1000, 700), true, false);
        assert!(pending.preferred_scale(150, Some((800, 600))));
        assert!(pending.preferred_scale(180, Some((800, 600))));
        assert!(!pending.preferred_scale(180, Some((800, 600))));
        assert_eq!(
            pending.take(),
            Some(Configure {
                size: size(1000, 700),
                scale_120: 180,
                remember: true,
            })
        );
        assert!(pending.preferred_scale(120, Some((1000, 700))));
        assert_eq!(
            pending.take(),
            Some(Configure {
                size: size(1000, 700),
                scale_120: 120,
                remember: false,
            })
        );
    }

    #[test]
    fn return_to_applied_geometry_skips_pool_but_preserves_final_persistence() {
        let mut pending = Pending::new(120);
        let applied = Some((800, 600));
        pending.configure(size(1200, 900), true, false);
        pending.preferred_scale(180, applied);
        pending.configure(size(800, 600), false, false);
        pending.preferred_scale(120, applied);
        pending.configure(size(800, 600), true, false);
        let configure = pending.take().unwrap();
        assert!(!configure.changes_geometry(applied, 120));
        assert!(configure.remember);
        assert!(configure.changes_geometry(applied, 150));
        assert!(configure.changes_geometry(None, 120));
    }

    #[test]
    fn live_grab_relayouts_once_per_interval_instead_of_once_per_sample() {
        let start = Instant::now();
        let mut pending = Pending::new(120);
        let mut applied = 0;
        // Nine seconds of dragging sampled every 65 ms, with the two-pixel
        // alternation that the old every-sample relayout amplified.
        for sample in 0..140_u64 {
            let now = start + Duration::from_millis(sample * 65);
            let width = 1_806 + i32::try_from(sample % 4).unwrap() - 2;
            pending.configure_at(size(u32::try_from(width).unwrap(), 1_928), false, true, now);
            assert!(pending.is_resizing());
            if pending.ready(now) {
                applied += 1;
                pending.take();
            }
        }
        // One relayout per interval rather than one per sample. Each commit
        // clears the deadline, so the next sample starts the following
        // interval: 9.1 s at 400 ms plus sample granularity commits ~17 times
        // against 140 samples, and the compositor previews every sample in
        // between.
        assert!(
            (15..=20).contains(&applied),
            "a live grab must be rate limited, not per sample (applied {applied})"
        );

        // The compositor's terminating configure carries the real final size
        // and applies it at once, without waiting out the settle window.
        let end = start + Duration::from_millis(140 * 65);
        pending.configure_at(size(1_754, 1_606), true, false, end);
        assert!(!pending.is_resizing());
        assert!(pending.ready(end));
        assert_eq!(
            pending.take(),
            Some(Configure {
                size: size(1_754, 1_606),
                scale_120: 120,
                remember: true,
            })
        );
        assert!(pending.deadline(end).is_none());
        assert!(pending.take().is_none());
    }

    #[test]
    fn stalled_grab_still_applies_its_last_size_by_the_interval() {
        let start = Instant::now();
        let mut pending = Pending::new(120);
        pending.configure_at(size(1_200, 800), true, true, start);
        assert!(!pending.ready(start));
        let settled = start + RESIZE_SETTLE;
        assert_eq!(pending.deadline(start), Some(settled));
        assert!(!pending.ready(start + RESIZE_SETTLE - Duration::from_millis(1)));
        assert!(pending.ready(settled));
        assert_eq!(pending.take().unwrap().size, size(1_200, 800));
    }

    #[test]
    fn later_grab_samples_do_not_push_the_interval_back() {
        let start = Instant::now();
        let mut pending = Pending::new(120);
        let geometry = size(1_100, 700);
        pending.configure_at(geometry, false, true, start);
        // A burst of changing samples must still commit on the original
        // interval. Restarting it per sample would starve the commit for the
        // whole drag, and a compositor that paces configures on that commit
        // would stop sending sizes and leave the window on a stretched preview.
        for sample in 1..40_u64 {
            pending.configure_at(
                size(1_100 + sample as u32, 700),
                false,
                true,
                start + Duration::from_millis(sample * 10),
            );
        }
        let edge = start + Duration::from_millis(390);
        assert_eq!(pending.deadline(edge), Some(start + RESIZE_SETTLE));
        assert!(!pending.ready(edge));
        assert!(pending.ready(start + RESIZE_SETTLE));
    }

    #[test]
    fn close_cancels_resize_before_allocation_and_removal_cancels_wakeup() {
        let object = TaskObjectId(7);
        let mut work = Tasks::default();
        let mut pending = Pending::new(120);
        pending.configure(size(1200, 900), true, true);
        work.mark(object, RESIZE | UNMAP);
        assert_eq!(work.pop(UNMAP), Some(object));
        pending.cancel();
        assert_eq!(work.pop(RESIZE), Some(object));
        assert!(pending.take().is_none());
        pending.configure(size(900, 700), false, false);
        work.mark(object, RESIZE);
        work.remove(object);
        assert_eq!(work.pop(RESIZE), None);
        assert!(!work.pending());
    }
}
