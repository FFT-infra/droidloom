// Droidloom Wayland Touchscreen Edge Gesture Recognizer
// Provides edge-swipe back navigation and top-edge fullscreen controls reveal.

use std::time::Duration;
use crate::TaskObjectId;

/// Margin in surface-local pixels from the left or right edge to intercept back gestures.
pub const EDGE_SWIPE_MARGIN: f64 = 28.0;

/// Margin in surface-local pixels from the top edge in fullscreen to intercept reveal gestures.
pub const TOP_EDGE_MARGIN: f64 = 24.0;

/// Distance in pixels required to confirm a horizontal back swipe.
pub const BACK_CONFIRM_DISPLACEMENT: f64 = 36.0;

/// Distance in pixels required to confirm a top-edge pull-down.
pub const TOP_CONFIRM_DISPLACEMENT: f64 = 24.0;

/// Vertical distance in pixels after which an edge touch is classified as vertical scrolling.
pub const SCROLL_DISAMBIGUATION_SLOP: f64 = 8.0;

/// Tap threshold: movements below this are treated as taps if released without confirming.
pub const TAP_THRESHOLD: f64 = 15.0;

/// Duration for which fullscreen controls stay revealed when triggered.
pub const FULLSCREEN_REVEAL_DURATION: Duration = Duration::from_millis(3500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeGestureKind {
    Back { is_left: bool },
    TopReveal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeGesturePhase {
    Pending,
    Confirmed,
    Cancelled,
}

#[derive(Clone, Copy, Debug)]
pub struct EdgeGestureTracker {
    #[allow(dead_code)]
    pub id: i32,
    pub object: TaskObjectId,
    pub kind: EdgeGestureKind,
    pub phase: EdgeGesturePhase,
    pub start_pos: (f64, f64),
    pub current_pos: (f64, f64),
    #[allow(dead_code)]
    pub serial: u32,
    pub pointer_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EdgeMotionResult {
    StayPending,
    ConfirmedBack,
    ConfirmedTopReveal,
    CancelScroll,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EdgeUpResult {
    TriggerBack,
    TriggerTopReveal,
    TapAtEdge { pos: (f64, f64) },
    None,
}

impl EdgeGestureTracker {
    /// Detects whether an initial touch down qualifies as a candidate edge gesture.
    pub fn new_candidate(
        id: i32,
        object: TaskObjectId,
        serial: u32,
        pointer_id: u32,
        pos: (f64, f64),
        window_size: (u32, u32),
        is_fullscreen: bool,
    ) -> Option<Self> {
        let (width, _height) = window_size;
        let width_f = width as f64;

        if is_fullscreen && pos.1 <= TOP_EDGE_MARGIN {
            return Some(Self {
                id,
                object,
                kind: EdgeGestureKind::TopReveal,
                phase: EdgeGesturePhase::Pending,
                start_pos: pos,
                current_pos: pos,
                serial,
                pointer_id,
            });
        }

        if pos.0 <= EDGE_SWIPE_MARGIN {
            return Some(Self {
                id,
                object,
                kind: EdgeGestureKind::Back { is_left: true },
                phase: EdgeGesturePhase::Pending,
                start_pos: pos,
                current_pos: pos,
                serial,
                pointer_id,
            });
        }

        if width_f > EDGE_SWIPE_MARGIN && pos.0 >= (width_f - EDGE_SWIPE_MARGIN) {
            return Some(Self {
                id,
                object,
                kind: EdgeGestureKind::Back { is_left: false },
                phase: EdgeGesturePhase::Pending,
                start_pos: pos,
                current_pos: pos,
                serial,
                pointer_id,
            });
        }

        None
    }

    /// Evaluates motion updates to classify gestures or cancel to in-app scrolling.
    pub fn on_motion(&mut self, new_pos: (f64, f64)) -> EdgeMotionResult {
        self.current_pos = new_pos;
        let dx = new_pos.0 - self.start_pos.0;
        let dy = new_pos.1 - self.start_pos.1;
        let abs_dx = dx.abs();
        let abs_dy = dy.abs();

        match self.kind {
            EdgeGestureKind::Back { is_left } => {
                if self.phase == EdgeGesturePhase::Confirmed {
                    return EdgeMotionResult::ConfirmedBack;
                }
                if self.phase == EdgeGesturePhase::Cancelled {
                    return EdgeMotionResult::CancelScroll;
                }

                // Vertical dominance check: if vertical movement exceeds slop and is greater
                // than or equal to horizontal movement, user is scrolling a list near edge.
                if abs_dy >= SCROLL_DISAMBIGUATION_SLOP && abs_dy >= abs_dx {
                    self.phase = EdgeGesturePhase::Cancelled;
                    return EdgeMotionResult::CancelScroll;
                }

                // Inward travel check: must move towards window center.
                let inward_dx = if is_left { dx } else { -dx };
                if inward_dx >= BACK_CONFIRM_DISPLACEMENT && abs_dx > 1.4 * abs_dy {
                    self.phase = EdgeGesturePhase::Confirmed;
                    return EdgeMotionResult::ConfirmedBack;
                }

                EdgeMotionResult::StayPending
            }
            EdgeGestureKind::TopReveal => {
                if self.phase == EdgeGesturePhase::Confirmed {
                    return EdgeMotionResult::ConfirmedTopReveal;
                }
                if self.phase == EdgeGesturePhase::Cancelled {
                    return EdgeMotionResult::CancelScroll;
                }

                if abs_dx >= 15.0 && abs_dx > dy {
                    self.phase = EdgeGesturePhase::Cancelled;
                    return EdgeMotionResult::CancelScroll;
                }

                if dy >= TOP_CONFIRM_DISPLACEMENT && dy > abs_dx {
                    self.phase = EdgeGesturePhase::Confirmed;
                    return EdgeMotionResult::ConfirmedTopReveal;
                }

                EdgeMotionResult::StayPending
            }
        }
    }

    /// Evaluates touch release to produce the final action.
    pub fn on_up(&self) -> EdgeUpResult {
        match self.phase {
            EdgeGesturePhase::Confirmed => match self.kind {
                EdgeGestureKind::Back { .. } => EdgeUpResult::TriggerBack,
                EdgeGestureKind::TopReveal => EdgeUpResult::TriggerTopReveal,
            },
            EdgeGesturePhase::Pending => {
                let dx = (self.current_pos.0 - self.start_pos.0).abs();
                let dy = (self.current_pos.1 - self.start_pos.1).abs();
                if dx < TAP_THRESHOLD && dy < TAP_THRESHOLD {
                    EdgeUpResult::TapAtEdge { pos: self.start_pos }
                } else {
                    EdgeUpResult::None
                }
            }
            EdgeGesturePhase::Cancelled => EdgeUpResult::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edge_swipe_back_left_confirmed() {
        let mut tracker = EdgeGestureTracker::new_candidate(
            1, TaskObjectId(100), 1, 0, (10.0, 500.0), (1920, 1080), false,
        ).expect("should be candidate");
        assert_eq!(tracker.kind, EdgeGestureKind::Back { is_left: true });
        assert_eq!(tracker.on_motion((20.0, 502.0)), EdgeMotionResult::StayPending);
        assert_eq!(tracker.on_motion((52.0, 505.0)), EdgeMotionResult::ConfirmedBack);
        assert_eq!(tracker.on_up(), EdgeUpResult::TriggerBack);
    }

    #[test]
    fn test_edge_swipe_back_right_confirmed() {
        let mut tracker = EdgeGestureTracker::new_candidate(
            1, TaskObjectId(100), 1, 0, (1910.0, 500.0), (1920, 1080), false,
        ).expect("should be candidate");
        assert_eq!(tracker.kind, EdgeGestureKind::Back { is_left: false });
        assert_eq!(tracker.on_motion((1860.0, 503.0)), EdgeMotionResult::ConfirmedBack);
        assert_eq!(tracker.on_up(), EdgeUpResult::TriggerBack);
    }

    #[test]
    fn test_vertical_scroll_cancelled_to_app() {
        let mut tracker = EdgeGestureTracker::new_candidate(
            1, TaskObjectId(100), 1, 0, (10.0, 500.0), (1920, 1080), false,
        ).expect("should be candidate");
        assert_eq!(tracker.on_motion((12.0, 485.0)), EdgeMotionResult::CancelScroll);
        assert_eq!(tracker.on_up(), EdgeUpResult::None);
    }

    #[test]
    fn test_tap_at_edge_triggers_tap() {
        let tracker = EdgeGestureTracker::new_candidate(
            1, TaskObjectId(100), 1, 0, (8.0, 300.0), (1920, 1080), false,
        ).expect("should be candidate");
        assert_eq!(tracker.on_up(), EdgeUpResult::TapAtEdge { pos: (8.0, 300.0) });
    }

    #[test]
    fn test_fullscreen_top_reveal() {
        let mut tracker = EdgeGestureTracker::new_candidate(
            1, TaskObjectId(100), 1, 0, (500.0, 10.0), (1920, 1080), true,
        ).expect("should be candidate in fullscreen");
        assert_eq!(tracker.kind, EdgeGestureKind::TopReveal);
        assert_eq!(tracker.on_motion((502.0, 42.0)), EdgeMotionResult::ConfirmedTopReveal);
        assert_eq!(tracker.on_up(), EdgeUpResult::TriggerTopReveal);
    }

    #[test]
    fn test_content_touch_not_candidate() {
        assert!(EdgeGestureTracker::new_candidate(
            1, TaskObjectId(100), 1, 0, (200.0, 300.0), (1920, 1080), false,
        ).is_none());
    }
}
