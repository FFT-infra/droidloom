//! Finger-only side-swipe Back recognition.
//!
//! The recognizer never consumes a stream before it commits: while a candidate
//! is pending the application receives every event, and the caller pilfers the
//! contact exactly once by cancelling it when the swipe confirms. Pen tools,
//! additional fingers, vertical scrolling and outward drags never produce a
//! Back. There is no animation, no timer and no cross-contact state.

use droidloom_denial_protocol::TaskObjectId;

/// Width of the edge band where a swipe may begin, in logical pixels.
pub const EDGE_MARGIN: f64 = 24.0;
/// Travel that classifies an undecided swipe.
pub const SLOP: f64 = 10.0;
/// Inward travel that commits a Back.
pub const CONFIRM_DISTANCE: f64 = 48.0;
/// Travel a fling must reach before velocity alone can commit it.
pub const FLING_TRAVEL: f64 = 24.0;
/// Logical pixels per second that qualify a fling.
pub const FLING_SPEED: f64 = 750.0;
/// Retreat toward the start edge that aborts a committed swipe before release.
pub const RETREAT_DISTANCE: f64 = 16.0;

/// One recognition update for a tracked contact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwipeUpdate {
    /// Undecided; the application keeps receiving the stream.
    Pending,
    /// The swipe committed; the caller cancels the contact exactly once.
    Confirmed,
    /// The candidate disqualified; the application keeps the whole stream.
    Cancelled,
}

/// A live view of the swipe, for drawing feedback while it is still undecided.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SwipeFeedback {
    pub from_left: bool,
    /// How far the contact has travelled away from its edge.
    pub inward: f64,
    /// The contact's position along the edge, in content coordinates.
    pub along: f64,
    /// Signed inward speed in logical pixels per second.
    pub speed: f64,
    pub confirmed: bool,
}

/// A single finger monitored for an inward edge swipe over one task.
#[derive(Clone, Copy, Debug)]
pub struct SwipeBackCandidate {
    object: TaskObjectId,
    /// Wayland touch id, kept as the protocol's contact identity.
    pointer_id: u32,
    from_left: bool,
    start: (f64, f64),
    last: (f64, f64),
    last_time_ms: u64,
    peak_speed: f64,
    inward_speed: f64,
    confirmed: bool,
    cancelled: bool,
    stealing: bool,
}

impl SwipeBackCandidate {
    /// Begin monitoring a touch that starts inside the content's edge band.
    ///
    /// Positions are content-local logical coordinates. Degenerate geometry,
    /// non-finite input and touches that start outside the band yield `None`.
    pub fn begin(
        object: TaskObjectId,
        pointer_id: u32,
        position: (f64, f64),
        window_size: (u32, u32),
        edge: f64,
        time_ms: u64,
    ) -> Option<Self> {
        let (width, height) = (f64::from(window_size.0), f64::from(window_size.1));
        if !position.0.is_finite() || !position.1.is_finite() || !edge.is_finite() {
            return None;
        }
        if width <= edge * 2.0 || height <= 0.0 || position.1 < 0.0 || position.1 > height {
            return None;
        }
        let from_left = if position.0 <= edge {
            true
        } else if position.0 >= width - edge {
            false
        } else {
            return None;
        };
        Some(Self {
            object,
            pointer_id,
            from_left,
            start: position,
            last: position,
            last_time_ms: time_ms,
            peak_speed: 0.0,
            inward_speed: 0.0,
            confirmed: false,
            cancelled: false,
            stealing: false,
        })
    }

    /// Which screen edge the contact entered from.
    pub fn from_left(&self) -> bool {
        self.from_left
    }

    /// The swipe's current geometry, for drawing feedback.
    pub fn feedback(&self) -> SwipeFeedback {
        let dx = self.last.0 - self.start.0;
        SwipeFeedback {
            from_left: self.from_left,
            inward: if self.from_left { dx } else { -dx },
            along: self.last.1,
            speed: self.inward_speed,
            confirmed: self.confirmed,
        }
    }

    /// Whether this tracker belongs to the given Wayland touch id.
    pub fn matches(&self, pointer_id: u32) -> bool {
        self.pointer_id == pointer_id
    }

    /// The task the swipe started on.
    pub fn object(&self) -> TaskObjectId {
        self.object
    }

    /// Whether the caller has already cancelled this contact for the swipe.
    pub fn stealing(&self) -> bool {
        self.stealing
    }

    /// Record that the caller pilfered the contact.
    pub fn mark_stealing(&mut self) {
        self.stealing = true;
    }

    /// Classify a motion sample.
    pub fn on_motion(&mut self, position: (f64, f64), time_ms: u64) -> SwipeUpdate {
        if !position.0.is_finite() || !position.1.is_finite() {
            self.cancelled = true;
            return SwipeUpdate::Cancelled;
        }
        let dt = time_ms.saturating_sub(self.last_time_ms);
        let sample_dx = position.0 - self.last.0;
        let sample_dy = position.1 - self.last.1;
        if dt > 0 {
            let speed = sample_dx.hypot(sample_dy) * 1000.0 / dt as f64;
            if speed > self.peak_speed {
                self.peak_speed = speed;
            }
            // Signed inward velocity, so the indicator can lean with the hand.
            self.inward_speed = if self.from_left {
                sample_dx * 1000.0 / dt as f64
            } else {
                -sample_dx * 1000.0 / dt as f64
            };
        }
        self.last = position;
        self.last_time_ms = time_ms;

        if self.cancelled {
            return SwipeUpdate::Cancelled;
        }
        let dx = position.0 - self.start.0;
        let dy = position.1 - self.start.1;
        let inward = if self.from_left { dx } else { -dx };

        if self.confirmed {
            if inward < RETREAT_DISTANCE {
                self.cancelled = true;
                return SwipeUpdate::Cancelled;
            }
            return SwipeUpdate::Confirmed;
        }
        // Vertical dominance hands the whole sequence back to the application.
        if dy.abs() >= SLOP && dy.abs() > dx.abs() {
            self.cancelled = true;
            return SwipeUpdate::Cancelled;
        }
        // Pulling outward toward the bezel is not a Back.
        if inward < -SLOP {
            self.cancelled = true;
            return SwipeUpdate::Cancelled;
        }
        if inward >= CONFIRM_DISTANCE
            || (inward >= FLING_TRAVEL && self.peak_speed >= FLING_SPEED)
        {
            self.confirmed = true;
            return SwipeUpdate::Confirmed;
        }
        SwipeUpdate::Pending
    }

    /// Finish the contact. `Some(object)` means one Back must be sent.
    ///
    /// Confirmation happens on motion samples only; a stream that never
    /// classified stays with the application, so release only re-checks the
    /// retreat threshold.
    pub fn on_up(&mut self, _time_ms: u64) -> Option<TaskObjectId> {
        if self.cancelled {
            return None;
        }
        let dx = self.last.0 - self.start.0;
        let inward = if self.from_left { dx } else { -dx };
        if inward < RETREAT_DISTANCE {
            return None;
        }
        self.confirmed.then_some(self.object)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: (u32, u32) = (800, 600);
    const TASK: TaskObjectId = TaskObjectId(7);

    fn begin_left() -> SwipeBackCandidate {
        SwipeBackCandidate::begin(TASK, 3, (10.0, 300.0), WINDOW, EDGE_MARGIN, 1_000).unwrap()
    }

    #[test]
    fn a_slow_inward_swipe_confirms_at_the_distance_threshold() {
        let mut candidate = begin_left();
        assert_eq!(candidate.on_motion((30.0, 302.0), 1_016), SwipeUpdate::Pending);
        assert_eq!(candidate.on_motion((60.0, 305.0), 1_032), SwipeUpdate::Confirmed);
        assert_eq!(candidate.on_up(1_050), Some(TASK));
    }

    #[test]
    fn the_right_edge_mirrors_the_left_edge() {
        let mut candidate =
            SwipeBackCandidate::begin(TASK, 4, (790.0, 200.0), WINDOW, EDGE_MARGIN, 0).unwrap();
        assert_eq!(candidate.on_motion((760.0, 202.0), 100), SwipeUpdate::Pending);
        assert_eq!(candidate.on_motion((735.0, 205.0), 220), SwipeUpdate::Confirmed);
        assert_eq!(candidate.on_up(260), Some(TASK));
    }

    #[test]
    fn vertical_scrolling_cancels_and_never_confirms() {
        let mut candidate = begin_left();
        assert_eq!(candidate.on_motion((11.0, 320.0), 16), SwipeUpdate::Cancelled);
        assert_eq!(candidate.on_motion((80.0, 320.0), 32), SwipeUpdate::Cancelled);
        assert_eq!(candidate.on_up(48), None);
    }

    #[test]
    fn outward_drag_is_not_a_back() {
        let mut candidate = begin_left();
        assert_eq!(candidate.on_motion((-4.0, 301.0), 16), SwipeUpdate::Cancelled);
        assert_eq!(candidate.on_up(24), None);
    }

    #[test]
    fn a_fast_fling_confirms_before_the_distance_threshold() {
        let mut candidate = begin_left();
        assert_eq!(candidate.on_motion((36.0, 301.0), 1_016), SwipeUpdate::Confirmed);
        assert_eq!(candidate.on_up(1_020), Some(TASK));
    }

    #[test]
    fn retreat_after_confirmation_aborts_the_back() {
        let mut candidate = begin_left();
        assert_eq!(candidate.on_motion((60.0, 302.0), 16), SwipeUpdate::Confirmed);
        assert_eq!(candidate.on_motion((12.0, 302.0), 32), SwipeUpdate::Cancelled);
        assert_eq!(candidate.on_up(48), None);
    }

    #[test]
    fn shallow_retreat_keeps_the_confirmation() {
        let mut candidate = begin_left();
        assert_eq!(candidate.on_motion((60.0, 302.0), 16), SwipeUpdate::Confirmed);
        assert_eq!(candidate.on_motion((30.0, 302.0), 32), SwipeUpdate::Confirmed);
        assert_eq!(candidate.on_up(48), Some(TASK));
    }

    #[test]
    fn a_motionless_tap_is_not_a_back() {
        let mut candidate = begin_left();
        assert_eq!(candidate.on_up(1_100), None);
    }

    #[test]
    fn invalid_samples_cancel_instead_of_guessing() {
        let mut candidate = begin_left();
        assert_eq!(
            candidate.on_motion((f64::NAN, 300.0), 16),
            SwipeUpdate::Cancelled
        );
        assert_eq!(candidate.on_up(24), None);
    }

    #[test]
    fn content_middle_and_degenerate_geometry_never_begin() {
        assert!(
            SwipeBackCandidate::begin(TASK, 1, (400.0, 300.0), WINDOW, EDGE_MARGIN, 0).is_none()
        );
        assert!(
            SwipeBackCandidate::begin(TASK, 1, (10.0, 700.0), WINDOW, EDGE_MARGIN, 0).is_none()
        );
        assert!(
            SwipeBackCandidate::begin(TASK, 1, (10.0, 300.0), (40, 600), EDGE_MARGIN, 0).is_none()
        );
    }

    #[test]
    fn identity_and_replay_are_deterministic() {
        let candidate = begin_left();
        assert!(candidate.matches(3));
        assert!(!candidate.matches(4));
        let trace = [(20.0, 301.0, 16u64), (50.0, 304.0, 32), (70.0, 306.0, 48)];
        let run = || {
            let mut candidate = begin_left();
            let mut updates = Vec::new();
            for (x, y, time) in trace {
                updates.push(candidate.on_motion((x, y), 1_000 + time));
            }
            (updates, candidate.on_up(1_060))
        };
        assert_eq!(run(), run());
    }
}
