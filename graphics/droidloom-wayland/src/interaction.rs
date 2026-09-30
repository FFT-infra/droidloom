//! Pure ownership and cancellation rules for client navigation controls.

use droidloom_denial_protocol::TaskObjectId;
use crate::chrome::{Action, BUTTON_SIZE};

#[derive(Debug)]
pub(super) struct ButtonPress<D> {
    pub device: D,
    pub id: i32,
    pub object: TaskObjectId,
    pub action: Action,
    pub epoch: u64,
    origin: (f64, f64),
    armed: bool,
}

impl<D: PartialEq> ButtonPress<D> {
    pub fn begin(device: D, id: i32, object: TaskObjectId, action: Action, epoch: u64,
        position: (f64, f64)) -> Option<Self> {
        inside(position).then_some(Self { device, id, object, action, epoch, origin: position, armed: true })
    }
    pub fn matches(&self, device: &D, id: i32) -> bool { &self.device == device && self.id == id }
    pub fn motion(&mut self, position: (f64, f64)) -> bool {
        let dx = position.0 - self.origin.0;
        let dy = position.1 - self.origin.1;
        if !inside(position) || !dx.is_finite() || !dy.is_finite() || dx * dx + dy * dy > 144.0 {
            self.armed = false;
        }
        self.armed
    }
    pub fn cancel(&mut self) { self.armed = false; }
    pub fn release(self, epoch: u64) -> Option<(TaskObjectId, Action)> {
        (self.armed && self.epoch == epoch).then_some((self.object, self.action))
    }
}

fn inside(position: (f64, f64)) -> bool {
    position.0.is_finite() && position.1.is_finite()
        && position.0 >= 0.0 && position.1 >= 0.0
        && position.0 < f64::from(BUTTON_SIZE) && position.1 < f64::from(BUTTON_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn press() -> ButtonPress<u32> {
        ButtonPress::begin(1, 2, TaskObjectId(3), Action::Back, 4, (20.0, 20.0)).unwrap()
    }
    #[test]
    fn a_tap_is_bound_to_device_contact_and_geometry() {
        let p = press();
        assert!(p.matches(&1, 2));
        assert!(!p.matches(&2, 2));
        assert_eq!(p.release(4), Some((TaskObjectId(3), Action::Back)));
        assert_eq!(press().release(5), None);
    }
    #[test]
    fn drag_away_and_return_cannot_rearm() {
        let mut p = press();
        assert!(!p.motion((60.0, 20.0)));
        assert!(!p.motion((20.0, 20.0)));
        assert_eq!(p.release(4), None);
    }
    #[test]
    fn invalid_coordinates_and_explicit_cancel_do_not_activate() {
        let mut p = press();
        assert!(!p.motion((f64::NAN, 20.0)));
        assert_eq!(p.release(4), None);
        let mut p = press(); p.cancel(); assert_eq!(p.release(4), None);
        assert!(ButtonPress::begin(1, 2, TaskObjectId(3), Action::Back, 4, (-1.0, 0.0)).is_none());
    }
}
