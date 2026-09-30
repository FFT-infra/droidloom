//! A one-dimensional spring, the primitive AOSP's edge panel animates with.
//!
//! Android drives every edge-panel property with `androidx.dynamicanimation`'s
//! `SpringForce`, described by a stiffness and a damping ratio. The constants
//! below are the ones Android's `EdgePanelParams` uses. The integration here is
//! a fixed-step semi-implicit Euler rather than the closed form: the step is
//! small enough that the difference is invisible, and the loop stays exact
//! under the variable frame times a Wayland client actually sees.

/// Substeps per second. Below the shortest frame a compositor will deliver.
const SUBSTEPS: f64 = 480.0;
/// Longest frame a spring will integrate in one call. A stalled event loop
/// resumes with one bounded step rather than an explosion.
const MAX_FRAME: f64 = 0.1;
/// `SpringForce.STIFFNESS_MEDIUM`.
pub const STIFFNESS_MEDIUM: f64 = 1_500.0;
/// `SpringForce.STIFFNESS_HIGH`.
pub const STIFFNESS_HIGH: f64 = 10_000.0;

#[derive(Clone, Copy, Debug)]
pub struct Spring {
    stiffness: f64,
    /// Damping coefficient, already expanded from the ratio.
    damping: f64,
    value: f64,
    target: f64,
    velocity: f64,
}

impl Spring {
    pub fn new(stiffness: f64, damping_ratio: f64, value: f64) -> Self {
        Self {
            stiffness,
            damping: 2.0 * damping_ratio * stiffness.sqrt(),
            value,
            target: value,
            velocity: 0.0,
        }
    }

    pub fn value(&self) -> f64 {
        self.value
    }

    /// Restart the spring at `value`, throwing away any motion in flight. Used
    /// while the finger owns the property: the spring must not fight the hand.
    pub fn track(&mut self, value: f64) {
        self.value = value;
        self.target = value;
        self.velocity = 0.0;
    }

    /// Aim at a new resting position, optionally with an initial kick.
    pub fn animate_to(&mut self, target: f64, velocity: f64) {
        self.target = target;
        self.velocity = velocity;
    }

    /// Whether the spring has come to rest close enough to stop redrawing.
    pub fn settled(&self, epsilon: f64) -> bool {
        (self.value - self.target).abs() <= epsilon && self.velocity.abs() <= epsilon * SUBSTEPS
    }

    /// Advance by a frame, substepping so a long frame cannot destabilise it.
    pub fn advance(&mut self, seconds: f64) {
        let seconds = seconds.clamp(0.0, MAX_FRAME);
        let steps = (seconds * SUBSTEPS).ceil().max(1.0) as u32;
        let step = seconds / f64::from(steps);
        for _ in 0..steps {
            let acceleration =
                -self.stiffness * (self.value - self.target) - self.damping * self.velocity;
            self.velocity += acceleration * step;
            self.value += self.velocity * step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settle(spring: &mut Spring, millis: u64) {
        for _ in 0..(millis / 16) {
            spring.advance(0.016);
        }
    }

    #[test]
    fn a_spring_reaches_its_target_and_stops() {
        let mut spring = Spring::new(STIFFNESS_MEDIUM, 0.75, 0.0);
        spring.animate_to(100.0, 0.0);
        settle(&mut spring, 1_000);
        assert!((spring.value() - 100.0).abs() < 0.5, "{:?}", spring.value());
        assert!(spring.settled(0.5));
    }

    #[test]
    fn a_bouncy_spring_overshoots_and_a_critical_one_does_not() {
        let mut bouncy = Spring::new(STIFFNESS_HIGH, 0.29, 0.0);
        bouncy.animate_to(100.0, 0.0);
        let mut peak: f64 = 0.0;
        for _ in 0..40 {
            bouncy.advance(0.016);
            peak = peak.max(bouncy.value());
        }
        assert!(peak > 100.0, "expected overshoot, peaked at {peak}");

        let mut critical = Spring::new(STIFFNESS_HIGH, 1.0, 0.0);
        critical.animate_to(100.0, 0.0);
        let mut peak: f64 = 0.0;
        for _ in 0..40 {
            critical.advance(0.016);
            peak = peak.max(critical.value());
        }
        assert!(peak <= 100.0 + 1e-6, "critical damping overshot to {peak}");
    }

    #[test]
    fn tracking_discards_motion_so_the_finger_always_wins() {
        let mut spring = Spring::new(STIFFNESS_MEDIUM, 0.75, 0.0);
        spring.animate_to(100.0, 0.0);
        spring.advance(0.016);
        spring.track(10.0);
        assert_eq!(spring.value(), 10.0);
        assert!(spring.settled(f64::EPSILON));
    }

    #[test]
    fn a_long_frame_is_substepped_instead_of_exploding() {
        let mut spring = Spring::new(STIFFNESS_HIGH, 1.0, 0.0);
        spring.animate_to(1.0, 0.0);
        // A one-second stall must still converge, not diverge.
        spring.advance(1.0);
        assert!(spring.value().is_finite());
        assert!(spring.value() >= 0.0 && spring.value() <= 1.0 + 1e-3, "{:?}", spring.value());
    }

    #[test]
    fn a_kick_launches_the_spring_towards_the_target() {
        let mut spring = Spring::new(STIFFNESS_HIGH, 1.0, 0.0);
        spring.animate_to(0.85, 3.0);
        spring.advance(0.016);
        assert!(spring.value() > 0.0, "the kick did not move it");
    }
}
