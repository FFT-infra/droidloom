//! Latest window geometry awaiting application after a Wayland dispatch batch.

use std::time::{Duration, Instant};

use droidloom_window_policy::LogicalSize;

/// Interval between lightweight surface commits during a live resize grab.
///
/// A compositor previews a live resize by scaling the last committed buffer to
/// the pointer, so intermediate geometry needs no work from this side: applying
/// it would retire a three-buffer pool, allocate another, and drive a full
/// Android task relayout for an image that is about to be replaced.
///
/// Denial paces its next configure on the client's commit. An empty
/// `wl_surface.commit` acknowledges that while preserving the attached buffer,
/// so it can keep the configure loop alive without forcing any Android work.
/// The interval is armed once per burst of samples; later samples coalesce into
/// the same commit without pushing it back.
const RESIZE_SETTLE: Duration = Duration::from_millis(400);

/// Safety net for a compositor that never sends a terminating resize configure.
///
/// Once the grab has gone quiet for this long, apply the latest geometry even
/// if no non-resizing configure arrives. Continuous drags keep refreshing the
/// deadline and therefore never take this path.
const RESIZE_GRAB_FALLBACK: Duration = Duration::from_secs(5);

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
    /// Earliest instant at which the next lightweight resize commit may run.
    apply_after: Option<Instant>,
    /// Latest interactive configure, used to detect a grab that never ends.
    last_interactive: Option<Instant>,
    /// The current interactive configure still needs a lightweight commit.
    commit_pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Acknowledge a live grab without changing the committed buffer.
    CommitExisting,
    /// Apply the final geometry through a complete Android task relayout.
    ApplyFinal(Configure),
}

impl Pending {
    pub fn new(scale_120: u32) -> Self {
        Self {
            size: None,
            scale_120,
            remember: false,
            interactive: false,
            apply_after: None,
            last_interactive: None,
            commit_pending: false,
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
    /// `interactive` marks a live resize grab. Intermediate geometry only
    /// requests a lightweight commit at [`RESIZE_SETTLE`]. A terminating
    /// configure applies the final size immediately; a grab that stays silent
    /// for [`RESIZE_GRAB_FALLBACK`] is also applied as a safety net.
    pub fn configure_at(
        &mut self,
        size: LogicalSize,
        remember: bool,
        interactive: bool,
        now: Instant,
    ) {
        if self.size == Some(size) && self.interactive == interactive {
            // Repeating one geometry must not push its own deadline forward,
            // or a chattering grab could postpone the commit indefinitely.
            if interactive {
                // Repeated identical configures are still paced events. They
                // must request a lightweight commit, but must not postpone an
                // already armed interval.
                self.last_interactive = Some(now);
                self.commit_pending = true;
                self.apply_after.get_or_insert(now + RESIZE_SETTLE);
            } else {
                self.apply_after = Some(now);
            }
            self.remember = remember;
            return;
        }
        self.size = Some(size);
        self.remember = remember;
        self.interactive = interactive;
        if !interactive {
            self.apply_after = Some(now);
        } else {
            self.last_interactive = Some(now);
            self.commit_pending = true;
            if self.apply_after.is_none() {
                // Arm once per burst: restarting the interval on every sample
                // would starve the lightweight acknowledgement.
                self.apply_after = Some(now + RESIZE_SETTLE);
            }
        }
    }

    /// Whether the compositor still owns an interactive resize of this window.
    #[cfg(test)]
    pub fn is_resizing(&self) -> bool {
        self.interactive
    }

    /// What, if anything, should happen to the queued geometry at `now`.
    pub fn action(&self, now: Instant) -> Option<Action> {
        let size = self.size?;
        if !self.interactive {
            return Some(Action::ApplyFinal(Configure {
                size,
                scale_120: self.scale_120,
                remember: self.remember,
            }));
        }
        if self
            .last_interactive
            .is_some_and(|last| now.saturating_duration_since(last) >= RESIZE_GRAB_FALLBACK)
        {
            return Some(Action::ApplyFinal(Configure {
                size,
                scale_120: self.scale_120,
                remember: self.remember,
            }));
        }
        (self.commit_pending && self.apply_after.is_none_or(|after| now >= after))
            .then_some(Action::CommitExisting)
    }

    /// Wake-up deadline of geometry that is still settling.
    pub fn deadline(&self, now: Instant) -> Option<Instant> {
        let commit = self
            .commit_pending
            .then_some(self.apply_after)
            .flatten()
            .filter(|after| *after > now);
        let fallback = if self.interactive {
            self.size
                .and(self.last_interactive)
                .map(|last| last + RESIZE_GRAB_FALLBACK)
                .filter(|after| *after > now)
        } else {
            None
        };
        commit.into_iter().chain(fallback).min()
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
            self.interactive = false;
            self.apply_after = Some(Instant::now());
            self.last_interactive = None;
            self.commit_pending = false;
        }
        self.size.is_some()
    }

    /// Consume the final configure after [`Action::ApplyFinal`] is selected.
    pub fn take(&mut self) -> Option<Configure> {
        let size = self.size.take()?;
        self.apply_after = None;
        self.interactive = false;
        self.last_interactive = None;
        self.commit_pending = false;
        let remember = std::mem::take(&mut self.remember);
        Some(Configure {
            size,
            scale_120: self.scale_120,
            remember,
        })
    }

    /// Record that a lightweight commit was sent.
    pub fn committed(&mut self) {
        self.commit_pending = false;
        self.apply_after = None;
    }

    pub fn cancel(&mut self) {
        self.size = None;
        self.remember = false;
        self.interactive = false;
        self.apply_after = None;
        self.last_interactive = None;
        self.commit_pending = false;
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
    fn long_live_grab_commits_without_intermediate_android_relayouts() {
        let start = Instant::now();
        let mut pending = Pending::new(120);
        let mut commits = Vec::new();
        let mut relayouts = Vec::new();
        // Nine seconds of dragging sampled every 65 ms, with the two-pixel
        // alternation that the old every-sample relayout amplified.
        for sample in 0..140_u64 {
            let now = start + Duration::from_millis(sample * 65);
            let width = 1_806 + i32::try_from(sample % 4).unwrap() - 2;
            pending.configure_at(size(u32::try_from(width).unwrap(), 1_928), false, true, now);
            assert!(pending.is_resizing());
            match pending.action(now) {
                Some(Action::CommitExisting) => {
                    commits.push(now);
                    pending.committed();
                }
                Some(Action::ApplyFinal(configure)) => relayouts.push((now, configure)),
                None => {}
            }
        }
        assert!(
            relayouts.is_empty(),
            "interactive geometry must not drive Android relayouts"
        );
        assert!(commits.len() >= 2, "paced commits keep Denial configuring");

        // Once Denial clears interactive state, one full relayout applies the
        // final geometry immediately.
        let end = start + Duration::from_millis(140 * 65);
        pending.configure_at(size(1_754, 1_606), true, false, end);
        assert!(!pending.is_resizing());
        assert_eq!(
            pending.action(end),
            Some(Action::ApplyFinal(Configure {
                size: size(1_754, 1_606),
                scale_120: 120,
                remember: true,
            }))
        );
        assert_eq!(pending.take().unwrap().size, size(1_754, 1_606));
        assert_eq!(pending.action(end), None);
        assert!(pending.deadline(end).is_none());
    }

    #[test]
    fn repeated_unchanged_geometry_still_acknowledges_each_paced_configure() {
        let start = Instant::now();
        let mut pending = Pending::new(120);
        let geometry = size(1_100, 700);
        pending.configure_at(geometry, false, true, start);
        assert_eq!(pending.action(start), None);
        let first = start + RESIZE_SETTLE;
        assert_eq!(pending.action(first), Some(Action::CommitExisting));
        pending.committed();
        assert_eq!(pending.action(first), None);

        // An unchanged configure is still a new paced event and must produce
        // its own acknowledgement.
        pending.configure_at(geometry, false, true, first);
        assert_eq!(
            pending.action(first + RESIZE_SETTLE - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            pending.action(first + RESIZE_SETTLE),
            Some(Action::CommitExisting)
        );
        pending.committed();
        assert_eq!(pending.action(first + RESIZE_SETTLE), None);
    }

    #[test]
    fn immediate_termination_applies_the_queued_final_size() {
        let start = Instant::now();
        let mut pending = Pending::new(120);
        pending.configure_at(size(1_200, 800), false, true, start);
        pending.configure_at(size(900, 700), true, false, start);
        assert_eq!(
            pending.action(start),
            Some(Action::ApplyFinal(Configure {
                size: size(900, 700),
                scale_120: 120,
                remember: true,
            }))
        );
    }

    #[test]
    fn stalled_grab_applies_once_after_the_fallback() {
        let start = Instant::now();
        let mut pending = Pending::new(120);
        pending.configure_at(size(1_200, 800), true, true, start);
        assert!(!matches!(
            pending.action(start),
            Some(Action::ApplyFinal(_))
        ));
        let fallback = start + RESIZE_GRAB_FALLBACK;
        assert_eq!(
            pending.action(fallback),
            Some(Action::ApplyFinal(Configure {
                size: size(1_200, 800),
                scale_120: 120,
                remember: true,
            }))
        );
        pending.take();
        assert_eq!(pending.action(fallback), None);
    }

    #[test]
    fn continuous_grab_samples_refresh_only_the_fallback() {
        let start = Instant::now();
        let mut pending = Pending::new(120);
        pending.configure_at(size(1_100, 700), false, true, start);
        for sample in 1..40_u64 {
            pending.configure_at(
                size(1_100 + sample as u32, 700),
                false,
                true,
                start + Duration::from_millis(sample * 100),
            );
        }
        let last = start + Duration::from_millis(3_900);
        assert_eq!(pending.deadline(last), Some(last + RESIZE_GRAB_FALLBACK));
        assert!(!matches!(pending.action(last), Some(Action::ApplyFinal(_))));
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
