//! Routed input state, independent of JNI, Binder and Android event objects.

use crate::input_keys::android_keycode;
use droidloom_input_protocol::{Payload, Sample};
use std::collections::{BTreeMap, BTreeSet};

const FLAG_CANCELED: i32 = 0x20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub task: i32,
    pub display: i32,
    pub incarnation: u64,
    pub token: u64,
    pub left: i32,
    pub top: i32,
}

impl Target {
    fn identity(self) -> (i32, i32, u64, u64) {
        (self.task, self.display, self.incarnation, self.token)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
pub struct Pointer {
    pub id: i32,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
}

/// Stack-only C ABI to the small Android dispatch adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
#[allow(clippy::struct_field_names)] // Android's eventTime/downTime terminology.
pub struct Event {
    pub kind: i32,
    pub display: i32,
    pub action: i32,
    pub key_code: i32,
    pub scan_code: i32,
    pub meta: i32,
    pub repeat: i32,
    pub flags: i32,
    pub count: i32,
    pub down_time: u64,
    pub event_time: u64,
    pub serial: u64,
    pub pointers: [Pointer; 32],
}

#[derive(Clone, Copy, Debug)]
pub struct RoutedEvent {
    pub target: Target,
    pub event: Event,
}

#[derive(Clone)]
struct Gesture {
    down_time: u64,
    last_time: u64,
    pointers: BTreeMap<u32, Pointer>,
}

#[derive(Clone, Copy)]
struct Key {
    down_time: u64,
    last_time: u64,
    code: i32,
}

#[derive(Clone, Default)]
struct Mouse {
    buttons: u32,
    x: f32,
    y: f32,
    down_time: u64,
    last_time: u64,
}

#[derive(Clone)]
struct Pen {
    tool: u32,
    down: bool,
    hovering: bool,
    buttons: u8,
    down_time: u64,
    last_time: u64,
    event: Event,
}

#[derive(Clone, Default)]
pub struct Engine {
    targets: BTreeMap<i32, Target>,
    gestures: BTreeMap<i32, Gesture>,
    keys: BTreeMap<(i32, u32), Key>,
    locks: BTreeMap<i32, i32>,
    mice: BTreeMap<i32, Mouse>,
    pens: BTreeMap<i32, Pen>,
    // Android resolves injected touchscreen input to one device per display,
    // not one device per task. Keep the first task's ownership until release;
    // contacts begun on another task must never become a new DOWN mid-contact.
    suppressed: BTreeSet<(i32, u32)>,
}

impl Engine {
    /// Update coordinates without interrupting a gesture; replacing its actual
    /// application or task incarnation cancels the old input before rebinding.
    pub fn publish(&mut self, target: Target, now: u64) -> Vec<RoutedEvent> {
        let cancelled = if self
            .targets
            .get(&target.task)
            .is_some_and(|old| old.identity() != target.identity())
        {
            self.cancel_task(target.task, now)
        } else {
            Vec::new()
        };
        self.targets.insert(target.task, target);
        cancelled
    }

    pub fn retire(&mut self, task: i32, incarnation: u64, now: u64) -> Vec<RoutedEvent> {
        if self
            .targets
            .get(&task)
            .is_none_or(|target| target.incarnation != incarnation)
        {
            return Vec::new();
        }
        let cancelled = self.cancel_task(task, now);
        self.targets.remove(&task);
        cancelled
    }

    pub fn reset(&mut self, now: u64) -> Vec<RoutedEvent> {
        self.suppressed.clear();
        let tasks: Vec<_> = self.targets.keys().copied().collect();
        tasks
            .into_iter()
            .flat_map(|task| self.cancel_task(task, now))
            .collect()
    }

    pub fn process(&mut self, sample: Sample) -> Result<Option<RoutedEvent>, &'static str> {
        let Some(target) = self.targets.get(&sample.task).copied().filter(|target| {
            target.display == sample.display && target.incarnation == sample.incarnation
        }) else {
            return Ok(None);
        };
        let event = match sample.payload {
            Payload::Mouse { .. } | Payload::Stylus { .. } => {
                return Err("pointer requires batch dispatch");
            }
            Payload::Touch { x, y, pressure } => {
                if self.suppress_touch(sample) {
                    return Ok(None);
                }
                self.touch(sample, target, x, y, pressure)?
            }
            Payload::Key { repeat, serial } => {
                // An unmapped but valid evdev key (for example a vendor Fn
                // key) must not cancel another held key or mouse gesture.
                if android_keycode(sample.code).is_none() {
                    return Ok(None);
                }
                self.key(sample, repeat, serial)?
            }
        };
        Ok(Some(RoutedEvent { target, event }))
    }

    /// A mouse button edge produces both the Android contact edge and its
    /// button notification. Keep them ordered on the same application token.
    pub fn process_batch(&mut self, sample: Sample) -> Result<Vec<RoutedEvent>, &'static str> {
        if matches!(sample.payload, Payload::Stylus { .. }) {
            return self.stylus(sample);
        }
        let Payload::Mouse {
            x,
            y,
            scroll_x,
            scroll_y,
        } = sample.payload
        else {
            return self
                .process(sample)
                .map(|event| event.into_iter().collect());
        };
        let Some(target) = self.targets.get(&sample.task).copied().filter(|target| {
            target.display == sample.display && target.incarnation == sample.incarnation
        }) else {
            return Ok(Vec::new());
        };
        let meta = self.meta(sample.task);
        let had_mouse = self.mice.contains_key(&sample.task);
        let mouse = self.mice.entry(sample.task).or_default();
        let delta = if had_mouse && sample.action == 1 {
            (x - mouse.x, y - mouse.y)
        } else {
            (0.0, 0.0)
        };
        mouse.x = x;
        mouse.y = y;
        mouse.last_time = sample.timestamp.max(mouse.last_time);
        let button = droidloom_input_protocol::mouse_button(sample.code).unwrap_or(0);
        let mut actions = Vec::with_capacity(2);
        match sample.action {
            0 => actions.push((9, 0)), // HOVER_ENTER
            1 => actions.push((if mouse.buttons == 0 { 7 } else { 2 }, 0)),
            2 => {
                if mouse.buttons != 0 {
                    mouse.buttons = 0;
                    actions.push((3, 0));
                }
                actions.push((10, 0)); // HOVER_EXIT
            }
            3 => {
                if button == 0 || mouse.buttons & button != 0 {
                    return Err("invalid or duplicate mouse press");
                }
                let first = mouse.buttons == 0;
                mouse.buttons |= button;
                if first {
                    mouse.down_time = mouse.last_time;
                    actions.push((0, 0)); // DOWN
                }
                actions.push((11, button)); // BUTTON_PRESS
            }
            4 => {
                if button == 0 || mouse.buttons & button == 0 {
                    return Ok(Vec::new());
                }
                mouse.buttons &= !button;
                actions.push((12, button)); // BUTTON_RELEASE
                if mouse.buttons == 0 {
                    actions.push((1, 0)); // UP
                }
            }
            5 => actions.push((8, 0)), // SCROLL
            6 => {
                if mouse.buttons != 0 {
                    mouse.buttons = 0;
                    actions.push((3, 0)); // CANCEL
                }
            }
            _ => return Err("mouse action"),
        }
        let mut result = Vec::with_capacity(actions.len());
        for (action, action_button) in actions {
            let mut event = Self::mouse_event(target, mouse, action, action_button, meta);
            // The fixed C ABI reserves a second pointer slot for wheel axes;
            // count stays one and this slot never becomes an Android pointer.
            event.pointers[1].x = scroll_x;
            event.pointers[1].y = scroll_y;
            event.pointers[2].x = delta.0;
            event.pointers[2].y = delta.1;
            result.push(RoutedEvent { target, event });
        }
        if matches!(sample.action, 2 | 6) {
            self.mice.remove(&sample.task);
        }
        // Match Android's CursorInputMapper: side buttons produce navigation
        // keys as well as button MotionEvents, including balanced cancellation.
        let navigation: Vec<_> = if matches!(sample.action, 2 | 6) {
            self.keys
                .keys()
                .filter_map(|(task, scan)| {
                    (*task == sample.task && matches!(scan, 0x113..=0x116)).then_some(*scan)
                })
                .collect()
        } else if matches!(sample.action, 3 | 4) && matches!(sample.code, 0x113..=0x116) {
            vec![sample.code]
        } else {
            Vec::new()
        };
        for scan in navigation {
            let key = Sample {
                code: scan,
                action: u8::from(sample.action != 3),
                payload: Payload::Key {
                    repeat: 0,
                    serial: 0,
                },
                ..sample
            };
            result.push(RoutedEvent {
                target,
                event: self.key(key, 0, 0)?,
            });
        }
        Ok(result)
    }

    fn stylus(&mut self, sample: Sample) -> Result<Vec<RoutedEvent>, &'static str> {
        let Some(target) = self.targets.get(&sample.task).copied().filter(|target| {
            target.display == sample.display && target.incarnation == sample.incarnation
        }) else {
            return Ok(Vec::new());
        };
        let Payload::Stylus {
            x,
            y,
            pressure,
            tilt_x,
            tilt_y,
            eraser,
            buttons,
        } = sample.payload
        else {
            return Err("stylus payload");
        };
        let meta = self.meta(sample.task);
        let mut result = Vec::new();
        if matches!(sample.action, 2 | 5) {
            return Ok(
                if self
                    .pens
                    .get(&sample.task)
                    .is_some_and(|pen| pen.tool == sample.code)
                {
                    self.cancel_pen(target, sample.timestamp)
                } else {
                    Vec::new()
                },
            );
        }
        // An enter replaces the old tool, cancelling any unfinished stroke.
        if sample.action != 0
            && self
                .pens
                .get(&sample.task)
                .is_some_and(|pen| pen.tool != sample.code)
        {
            return Ok(Vec::new());
        }
        if sample.action == 0 {
            result.extend(self.cancel_pen(target, sample.timestamp));
        }
        if !self.pens.contains_key(&sample.task) {
            if matches!(sample.action, 2 | 4 | 5 | 6) {
                return Ok(result);
            }
            let event = Event {
                kind: 8,
                display: target.display,
                count: 1,
                ..Event::default()
            };
            self.pens.insert(
                sample.task,
                Pen {
                    tool: sample.code,
                    down: false,
                    hovering: false,
                    buttons: 0,
                    down_time: sample.timestamp,
                    last_time: sample.timestamp,
                    event,
                },
            );
        }
        let pen = self.pens.get_mut(&sample.task).unwrap();
        pen.last_time = pen.last_time.max(sample.timestamp);
        pen.event.key_code = if eraser { 4 } else { 2 }; // Android ToolType.
        pen.event.meta = meta;
        pen.event.event_time = pen.last_time;
        pen.event.pointers[0] = Pointer {
            id: 0,
            x: x + target.left as f32,
            y: y + target.top as f32,
            pressure: if pen.down { pressure } else { 0.0 },
        };
        // Additional axes occupy an unused C ABI slot, never a second pointer.
        let tx = tilt_x.clamp(-89.999, 89.999).to_radians().tan();
        let ty = tilt_y.clamp(-89.999, 89.999).to_radians().tan();
        pen.event.pointers[1].x = tx.hypot(ty).atan();
        pen.event.pointers[1].y = if tx == 0.0 && ty == 0.0 {
            0.0
        } else {
            (-tx).atan2(ty)
        };
        let mut actions = Vec::new();
        match sample.action {
            0 => {
                pen.hovering = true;
                actions.push((9, 0));
            }
            1 => {
                if !pen.down && !pen.hovering {
                    pen.hovering = true;
                    actions.push((9, 0));
                }
                actions.push((if pen.down { 2 } else { 7 }, 0));
            }
            2 | 5 => {
                if pen.down {
                    actions.push((3, 0));
                    pen.down = false;
                }
                if pen.hovering {
                    actions.push((10, 0));
                    pen.hovering = false;
                }
            }
            3 => {
                if pen.down {
                    actions.push((3, 0));
                }
                if pen.hovering {
                    actions.push((10, 0));
                    pen.hovering = false;
                }
                pen.down = true;
                pen.down_time = pen.last_time;
                actions.push((0, 0));
            }
            4 => {
                if pen.down {
                    pen.down = false;
                    actions.push((1, 0));
                }
                if !pen.hovering {
                    pen.hovering = true;
                    actions.push((9, 0));
                }
            }
            6 => {
                for bit in [1, 2] {
                    if (pen.buttons ^ buttons) & bit != 0 {
                        actions.push((
                            if buttons & bit != 0 { 11 } else { 12 },
                            i32::from(bit) << 5,
                        ));
                    }
                }
            }
            _ => return Err("stylus action"),
        }
        if sample.action == 6 {
            pen.buttons = buttons;
        }
        for (action, button) in actions {
            let mut event = pen.event;
            event.action = action;
            event.scan_code = button;
            // Motion ABI flags carry the button state, not the cancel marker:
            // the native adapter derives MotionFlag::CANCELED from the action.
            event.flags = i32::from(buttons) << 5;
            event.down_time = if matches!(action, 7 | 9 | 10) {
                pen.last_time
            } else {
                pen.down_time
            };
            event.pointers[0].pressure = if matches!(action, 0 | 2 | 11 | 12) && pen.down {
                pressure
            } else {
                0.0
            };
            result.push(RoutedEvent { target, event });
        }
        if matches!(sample.action, 2 | 5) {
            self.pens.remove(&sample.task);
        }
        Ok(result)
    }

    fn cancel_pen(&mut self, target: Target, now: u64) -> Vec<RoutedEvent> {
        let Some(pen) = self.pens.remove(&target.task) else {
            return Vec::new();
        };
        let mut event = pen.event;
        event.down_time = pen.down_time;
        event.event_time = now.max(pen.last_time);
        event.pointers[0].pressure = 0.0;
        event.flags = i32::from(pen.buttons) << 5;
        event.scan_code = 0;
        let mut result = Vec::new();
        if pen.down {
            event.action = 3;
            result.push(RoutedEvent { target, event });
        }
        if pen.hovering {
            event.action = 10;
            result.push(RoutedEvent { target, event });
        }
        result
    }

    #[allow(clippy::cast_precision_loss)]
    fn mouse_event(target: Target, mouse: &Mouse, action: i32, button: u32, meta: i32) -> Event {
        let mut event = Event {
            kind: 7,
            display: target.display,
            action,
            key_code: button as i32, // Mouse actionButton, not a key identity.
            flags: mouse.buttons as i32, // Mouse buttonState, not key flags.
            meta,
            count: 1,
            down_time: if mouse.buttons == 0 && matches!(action, 7..=10) {
                mouse.last_time
            } else {
                mouse.down_time
            },
            event_time: mouse.last_time,
            ..Event::default()
        };
        event.pointers[0] = Pointer {
            id: 0,
            x: mouse.x + target.left as f32,
            y: mouse.y + target.top as f32,
            pressure: f32::from(mouse.buttons != 0),
        };
        event
    }

    fn suppress_touch(&mut self, sample: Sample) -> bool {
        let identity = (sample.task, sample.code);
        if self.suppressed.contains(&identity) {
            if sample.action == 2 {
                self.suppressed.remove(&identity);
            } else if sample.action == 3 {
                self.suppressed.retain(|(task, _)| *task != sample.task);
            }
            return true;
        }
        if self.gestures.keys().any(|task| {
            *task != sample.task
                && self
                    .targets
                    .get(task)
                    .is_some_and(|t| t.display == sample.display)
        }) {
            if sample.action == 0 {
                self.suppressed.insert(identity);
            }
            return true;
        }
        false
    }

    /// Commit only the retirement Android accepted. Failed cancellations keep
    /// their original pointer set/token available for a later reset attempt.
    pub fn acknowledge_cancel(&mut self, event: &RoutedEvent) {
        if event.event.kind == 1 {
            self.gestures.remove(&event.target.task);
        } else if event.event.kind == 2 {
            if let Ok(scan_code) = u32::try_from(event.event.scan_code) {
                self.keys.remove(&(event.target.task, scan_code));
            }
        }
    }

    fn touch(
        &mut self,
        sample: Sample,
        target: Target,
        x: f32,
        y: f32,
        pressure: f32,
    ) -> Result<Event, &'static str> {
        if sample.code > droidloom_input_protocol::MAX_POINTER_ID || sample.action > 3 {
            return Err("invalid touch sample");
        }
        if sample.action == 0 {
            if self
                .gestures
                .get(&sample.task)
                .is_some_and(|g| g.pointers.contains_key(&sample.code))
            {
                return Err("duplicate touch down");
            }
            self.gestures.entry(sample.task).or_insert_with(|| Gesture {
                down_time: sample.timestamp,
                last_time: sample.timestamp,
                pointers: BTreeMap::new(),
            });
        }
        let gesture = self
            .gestures
            .get_mut(&sample.task)
            .ok_or("touch has no down")?;
        if sample.action != 0 && !gesture.pointers.contains_key(&sample.code) {
            return Err("touch pointer has no down");
        }
        gesture.pointers.insert(
            sample.code,
            Pointer {
                id: i32::try_from(sample.code).map_err(|_| "pointer id")?,
                x,
                y,
                pressure,
            },
        );
        let index = gesture
            .pointers
            .keys()
            .position(|id| *id == sample.code)
            .ok_or("pointer missing")?;
        let count = gesture.pointers.len();
        let action = match (sample.action, count) {
            (0, 1) => 0,
            (0, _) => 5 | (i32::try_from(index).unwrap() << 8),
            (1, _) => 2,
            (2, 1) => 1,
            (2, _) => 6 | (i32::try_from(index).unwrap() << 8),
            (3, _) => 3,
            _ => return Err("touch action"),
        };
        let event = Self::motion(target, gesture, action, sample.timestamp);
        if sample.action == 2 {
            gesture.pointers.remove(&sample.code);
        }
        if sample.action == 3 || gesture.pointers.is_empty() {
            self.gestures.remove(&sample.task);
        }
        Ok(event)
    }

    #[allow(clippy::cast_precision_loss)] // Native MotionEvent coordinates are f32.
    fn motion(target: Target, gesture: &mut Gesture, action: i32, now: u64) -> Event {
        gesture.last_time = now.max(gesture.last_time);
        let mut event = Event {
            kind: 1,
            display: target.display,
            action,
            flags: if action == 3 { FLAG_CANCELED } else { 0 },
            count: i32::try_from(gesture.pointers.len()).unwrap(),
            down_time: gesture.down_time,
            event_time: gesture.last_time,
            ..Event::default()
        };
        for (slot, pointer) in event.pointers.iter_mut().zip(gesture.pointers.values()) {
            *slot = Pointer {
                x: pointer.x + target.left as f32,
                y: pointer.y + target.top as f32,
                pressure: if action == 1 || action == 3 {
                    0.0
                } else {
                    pointer.pressure
                },
                ..*pointer
            };
        }
        event
    }

    fn key(&mut self, sample: Sample, repeat: u16, serial: u64) -> Result<Event, &'static str> {
        let code = android_keycode(sample.code).ok_or("unsupported evdev key")?;
        let identity = (sample.task, sample.code);
        let previous = self.keys.get(&identity).copied();
        let pressed = sample.action == 0;
        if pressed && repeat == 0 && previous.is_some() {
            return Err("duplicate key down");
        }
        if (!pressed || repeat != 0) && previous.is_none() {
            return Err("key has no down");
        }
        let key = Key {
            down_time: previous.map_or(sample.timestamp, |key| key.down_time),
            last_time: previous.map_or(sample.timestamp, |key| sample.timestamp.max(key.last_time)),
            code,
        };
        if pressed {
            self.keys.insert(identity, key);
            if repeat == 0 {
                let bit = match code {
                    115 => 0x10_0000,
                    143 => 0x20_0000,
                    116 => 0x40_0000,
                    _ => 0,
                };
                *self.locks.entry(sample.task).or_default() ^= bit;
            }
        } else {
            self.keys.remove(&identity);
        }
        Ok(Event {
            kind: 2,
            display: sample.display,
            action: i32::from(!pressed),
            key_code: code,
            scan_code: i32::try_from(sample.code).map_err(|_| "key scan code")?,
            repeat: i32::from(repeat),
            meta: self.meta(sample.task),
            flags: 0x08,
            down_time: key.down_time,
            event_time: key.last_time,
            serial,
            ..Event::default()
        })
    }

    fn meta(&self, task: i32) -> i32 {
        let mut meta = self.locks.get(&task).copied().unwrap_or(0);
        for ((owner, _), key) in &self.keys {
            if *owner != task {
                continue;
            }
            meta |= match key.code {
                59 => 0x41,
                60 => 0x81,
                57 => 0x12,
                58 => 0x22,
                113 => 0x3000,
                114 => 0x5000,
                117 => 0x3_0000,
                118 => 0x5_0000,
                _ => 0,
            };
        }
        meta
    }

    fn cancel_task(&mut self, task: i32, now: u64) -> Vec<RoutedEvent> {
        self.suppressed.retain(|(owner, _)| *owner != task);
        let Some(target) = self.targets.get(&task).copied() else {
            return Vec::new();
        };
        let mut cancelled = Vec::new();
        cancelled.extend(self.cancel_pen(target, now));
        if let Some(mut mouse) = self.mice.remove(&task) {
            let action = if mouse.buttons == 0 { 10 } else { 3 };
            mouse.buttons = 0;
            mouse.last_time = now.max(mouse.last_time);
            cancelled.push(RoutedEvent {
                target,
                event: Self::mouse_event(target, &mouse, action, 0, self.meta(task)),
            });
        }
        if let Some(mut gesture) = self.gestures.remove(&task) {
            cancelled.push(RoutedEvent {
                target,
                event: Self::motion(target, &mut gesture, 3, now),
            });
        }
        let keys: Vec<_> = self
            .keys
            .keys()
            .filter(|(owner, _)| *owner == task)
            .copied()
            .collect();
        for identity in keys {
            let key = self.keys.remove(&identity).unwrap();
            cancelled.push(RoutedEvent {
                target,
                event: Event {
                    kind: 2,
                    display: target.display,
                    action: 1,
                    key_code: key.code,
                    scan_code: i32::try_from(identity.1).unwrap(),
                    flags: 0x28,
                    meta: self.meta(task),
                    down_time: key.down_time,
                    event_time: now.max(key.last_time),
                    ..Event::default()
                },
            });
        }
        self.locks.remove(&task);
        cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pen(action: u8, tool: u32, eraser: bool, buttons: u8) -> Sample {
        let record = droidloom_input_protocol::encode_stylus(
            0,
            42,
            100,
            7,
            droidloom_input_protocol::Stylus {
                tool,
                action,
                x_fixed: 3 << 16,
                y_fixed: 5 << 16,
                pressure: 32768,
                tilt_x: 30,
                tilt_y: -45,
                eraser,
                buttons,
            },
        )
        .unwrap();
        let droidloom_input_protocol::Record::Input(sample) =
            droidloom_input_protocol::decode(&record).unwrap()
        else {
            panic!("pen sample")
        };
        sample
    }

    #[test]
    fn native_pen_preserves_hover_stroke_eraser_and_axes() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        let enter = engine.process_batch(pen(0, 9, true, 0)).unwrap();
        assert_eq!(
            (
                enter[0].event.kind,
                enter[0].event.action,
                enter[0].event.key_code
            ),
            (8, 9, 4)
        );
        assert_eq!(enter[0].event.pointers[0].pressure, 0.0);
        let hover = engine.process_batch(pen(1, 9, true, 0)).unwrap();
        assert_eq!(hover[0].event.action, 7);
        assert!(hover[0].event.pointers[1].x > 0.0);
        let down = engine.process_batch(pen(3, 9, true, 0)).unwrap();
        assert_eq!(
            down.iter().map(|r| r.event.action).collect::<Vec<_>>(),
            [10, 0]
        );
        assert!((down[1].event.pointers[0].pressure - 0.5).abs() < 0.001);
        assert_eq!(
            engine.process_batch(pen(1, 9, true, 0)).unwrap()[0]
                .event
                .action,
            2
        );
        let up = engine.process_batch(pen(4, 9, true, 0)).unwrap();
        assert_eq!(
            up.iter().map(|r| r.event.action).collect::<Vec<_>>(),
            [1, 9]
        );
        assert_eq!(
            engine.process_batch(pen(2, 9, true, 0)).unwrap()[0]
                .event
                .action,
            10
        );
        assert!(engine.reset(200).is_empty());
    }

    #[test]
    fn pen_route_retirement_cancels_only_its_own_stroke() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.publish(
            Target {
                task: 43,
                token: 11,
                ..target()
            },
            1,
        );
        engine.process_batch(pen(0, 9, false, 0)).unwrap();
        engine.process_batch(pen(3, 9, false, 0)).unwrap();
        engine
            .process(Sample {
                task: 43,
                ..key(29, 0, 0)
            })
            .unwrap();
        let cancelled = engine.retire(42, 7, 200);
        assert_eq!(cancelled.len(), 1);
        assert_eq!(
            (
                cancelled[0].target.token,
                cancelled[0].event.kind,
                cancelled[0].event.action,
                cancelled[0].event.flags,
            ),
            (9, 8, 3, 0)
        );
        assert_eq!(engine.reset(300)[0].target.task, 43);
        assert!(
            engine
                .process_batch(pen(1, 9, false, 0))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn pen_buttons_and_replaced_tools_do_not_leave_a_stroke_held() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.process_batch(pen(0, 9, false, 0)).unwrap();
        engine.process_batch(pen(3, 9, false, 1)).unwrap();
        let button = engine.process_batch(pen(6, 9, false, 1)).unwrap();
        assert_eq!(
            (
                button[0].event.action,
                button[0].event.scan_code,
                button[0].event.flags
            ),
            (11, 32, 32)
        );
        let release = engine.process_batch(pen(6, 9, false, 0)).unwrap();
        assert_eq!(
            (release[0].event.action, release[0].event.scan_code),
            (12, 32)
        );
        let replace = engine.process_batch(pen(0, 10, false, 0)).unwrap();
        assert_eq!(
            replace.iter().map(|r| r.event.action).collect::<Vec<_>>(),
            [3, 9]
        );
        // Late proximity-out from the first tool must not cancel its successor.
        assert!(
            engine
                .process_batch(pen(2, 9, false, 0))
                .unwrap()
                .is_empty()
        );
        assert_eq!(engine.reset(200)[0].event.action, 10);
    }

    #[test]
    fn remote_keyboard_preserves_keypad_and_numlock_semantics() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        let lock = engine.process(key(69, 0, 0)).unwrap().unwrap().event;
        assert_eq!(lock.flags, 0x08);
        assert_eq!(lock.flags & 0x40, 0);
        assert_eq!((lock.key_code, lock.meta & 0x20_0000), (143, 0x20_0000));
        engine.process(key(69, 1, 0)).unwrap();
        for (scan, android) in [
            (55, 155),
            (71, 151),
            (78, 157),
            (82, 144),
            (83, 158),
            (98, 154),
            (117, 161),
        ] {
            let event = engine.process(key(scan, 0, 0)).unwrap().unwrap().event;
            assert_eq!(event.key_code, android);
            assert_ne!(event.meta & 0x20_0000, 0);
            engine.process(key(scan, 1, 0)).unwrap();
        }
    }

    #[test]
    fn an_unmapped_vendor_key_does_not_cancel_a_remote_control_modifier() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.process(key(29, 0, 0)).unwrap();
        assert!(engine.process(key(0x2ff, 0, 0)).unwrap().is_none());
        assert!(engine.process(key(0x2ff, 1, 0)).unwrap().is_none());
        let a = engine.process(key(30, 0, 0)).unwrap().unwrap().event;
        assert_ne!(a.meta & 0x1000, 0);
        assert_eq!(
            engine
                .process(key(29, 1, 0))
                .unwrap()
                .unwrap()
                .event
                .key_code,
            113
        );
    }

    fn mouse(action: u8, button: u32) -> Sample {
        Sample {
            action,
            code: button,
            payload: Payload::Mouse {
                x: 2.5,
                y: 3.0,
                scroll_x: 0.5,
                scroll_y: -1.0,
            },
            ..touch(0, 0)
        }
    }

    #[test]
    fn mouse_buttons_hover_wheel_and_release_keep_native_semantics() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        let hover = engine.process_batch(mouse(0, 0)).unwrap();
        assert_eq!((hover[0].event.kind, hover[0].event.action), (7, 9));
        assert_eq!(hover[0].event.down_time, 100);
        let press = engine.process_batch(mouse(3, 0x110)).unwrap();
        assert_eq!(
            press.iter().map(|e| e.event.action).collect::<Vec<_>>(),
            [0, 11]
        );
        assert_eq!(press[1].event.key_code, 1);
        assert_eq!(
            (press[0].event.pointers[0].x, press[0].event.pointers[0].y),
            (32.5, 43.0)
        );
        let right = engine.process_batch(mouse(3, 0x111)).unwrap();
        assert_eq!(
            (right.len(), right[0].event.action, right[0].event.flags),
            (1, 11, 3)
        );
        let motion = engine.process_batch(mouse(1, 0)).unwrap();
        assert_eq!(motion[0].event.action, 2);
        let scroll = engine.process_batch(mouse(5, 0)).unwrap();
        assert_eq!(
            (
                scroll[0].event.action,
                scroll[0].event.pointers[1].x,
                scroll[0].event.pointers[1].y
            ),
            (8, 0.5, -1.0)
        );
        let left_up = engine.process_batch(mouse(4, 0x110)).unwrap();
        assert_eq!(
            (
                left_up.len(),
                left_up[0].event.action,
                left_up[0].event.flags
            ),
            (1, 12, 2)
        );
        let right_up = engine.process_batch(mouse(4, 0x111)).unwrap();
        assert_eq!(
            right_up.iter().map(|e| e.event.action).collect::<Vec<_>>(),
            [12, 1]
        );
        assert_eq!(
            engine.process_batch(mouse(1, 0)).unwrap()[0].event.action,
            7
        );
        assert_eq!(
            engine.process_batch(mouse(2, 0)).unwrap()[0].event.action,
            10
        );
    }

    #[test]
    fn mouse_disconnect_and_rebinding_cancel_only_the_owning_task() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.process_batch(mouse(3, 0x112)).unwrap();
        engine.publish(
            Target {
                task: 43,
                token: 10,
                ..target()
            },
            1,
        );
        engine
            .process(Sample {
                task: 43,
                ..key(30, 0, 0)
            })
            .unwrap();
        let cancelled = engine.publish(
            Target {
                incarnation: 8,
                token: 11,
                ..target()
            },
            200,
        );
        assert_eq!(
            (
                cancelled.len(),
                cancelled[0].event.kind,
                cancelled[0].event.action
            ),
            (1, 7, 3)
        );
        assert_eq!(cancelled[0].target.token, 9);
        assert!(engine.process_batch(mouse(3, 0x110)).unwrap().is_empty());
        let remaining = engine.reset(300);
        assert_eq!(
            (
                remaining.len(),
                remaining[0].target.task,
                remaining[0].event.kind
            ),
            (1, 43, 2)
        );
    }

    #[test]
    fn mouse_relative_motion_and_side_navigation_balance_on_leave() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.process_batch(mouse(0, 0)).unwrap();
        let moved = engine
            .process_batch(Sample {
                payload: Payload::Mouse {
                    x: 5.0,
                    y: 7.0,
                    scroll_x: 0.0,
                    scroll_y: 0.0,
                },
                ..mouse(1, 0)
            })
            .unwrap();
        assert_eq!(
            (moved[0].event.pointers[2].x, moved[0].event.pointers[2].y),
            (2.5, 4.0)
        );
        let back = engine.process_batch(mouse(3, 0x113)).unwrap();
        assert_eq!(
            (
                back[2].event.kind,
                back[2].event.key_code,
                back[2].event.action
            ),
            (2, 4, 0)
        );
        let leave = engine.process_batch(mouse(2, 0)).unwrap();
        assert_eq!(
            leave
                .iter()
                .map(|event| (event.event.kind, event.event.action))
                .collect::<Vec<_>>(),
            [(7, 3), (7, 10), (2, 1)]
        );
        assert!(engine.reset(200).is_empty());
    }
    fn target() -> Target {
        Target {
            task: 42,
            display: 0,
            incarnation: 7,
            token: 9,
            left: 30,
            top: 40,
        }
    }
    fn touch(id: u32, action: u8) -> Sample {
        Sample {
            display: 0,
            task: 42,
            incarnation: 7,
            timestamp: 100,
            action,
            code: id,
            payload: Payload::Touch {
                x: 2.5,
                y: 3.0,
                pressure: 1.0,
            },
        }
    }
    fn key(scan: u32, action: u8, repeat: u16) -> Sample {
        Sample {
            payload: Payload::Key { repeat, serial: 55 },
            code: scan,
            action,
            ..touch(0, 0)
        }
    }
    #[test]
    fn multi_touch_indices_and_bounds_follow_the_same_live_task() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        let first = engine.process(touch(7, 0)).unwrap().unwrap().event;
        assert_eq!((first.action, first.count), (0, 1));
        assert_eq!((first.pointers[0].x, first.pointers[0].y), (32.5, 43.0));
        let second = engine.process(touch(2, 0)).unwrap().unwrap().event;
        assert_eq!((second.action, second.count), (5, 2));
        assert_eq!((second.pointers[0].id, second.pointers[1].id), (2, 7));
        assert!(
            engine
                .publish(
                    Target {
                        left: 50,
                        ..target()
                    },
                    101
                )
                .is_empty()
        );
        let moved = engine
            .process(Sample {
                timestamp: 90,
                ..touch(7, 1)
            })
            .unwrap()
            .unwrap()
            .event;
        assert_eq!((moved.pointers[1].x, moved.event_time), (52.5, 100));
        let up = engine.process(touch(7, 2)).unwrap().unwrap().event;
        assert_eq!(up.action, 6 | 1 << 8);
        assert_eq!(
            engine.process(touch(2, 2)).unwrap().unwrap().event.action,
            1
        );
        assert!(engine.process(touch(2, 1)).is_err());
    }
    #[test]
    fn token_and_incarnation_changes_cancel_old_input_and_reject_stale_samples() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.process(touch(0, 0)).unwrap();
        engine.process(key(30, 0, 0)).unwrap();
        let new = Target {
            incarnation: 8,
            token: 10,
            ..target()
        };
        let cancelled = engine.publish(new, 200);
        assert_eq!(cancelled.len(), 2);
        assert!(cancelled.iter().all(|event| event.target.token == 9));
        assert_eq!(cancelled[0].event.action, 3);
        assert_eq!(cancelled[0].event.flags & FLAG_CANCELED, FLAG_CANCELED);
        assert_eq!(cancelled[1].event.flags, 0x28);
        assert_eq!(cancelled[1].event.flags & 0x20, 0x20);
        assert!(engine.process(touch(0, 0)).unwrap().is_none());
        assert!(engine.retire(42, 7, 201).is_empty());
        assert!(
            engine
                .process(Sample {
                    incarnation: 8,
                    ..touch(0, 0)
                })
                .unwrap()
                .is_some()
        );
        assert_eq!(engine.publish(Target { token: 11, ..new }, 202).len(), 1);
    }
    #[test]
    fn modifiers_are_isolated_and_releasing_one_shift_keeps_the_other() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.publish(
            Target {
                task: 43,
                ..target()
            },
            1,
        );
        engine.process(key(42, 0, 0)).unwrap();
        engine.process(key(54, 0, 0)).unwrap();
        let left_up = engine.process(key(42, 1, 0)).unwrap().unwrap().event;
        assert_eq!(left_up.meta, 0x81);
        let other = engine
            .process(Sample {
                task: 43,
                ..key(30, 0, 0)
            })
            .unwrap()
            .unwrap()
            .event;
        assert_eq!(other.meta, 0);
        assert_eq!(engine.reset(200).len(), 2);
        assert!(engine.process(key(54, 1, 0)).is_err());
    }
    #[test]
    fn repeats_keep_down_time_and_route_trace_serial() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        assert!(engine.process(key(30, 0, 1)).is_err());
        engine.process(key(30, 0, 0)).unwrap();
        let repeated = engine
            .process(Sample {
                timestamp: 300,
                ..key(30, 0, 2)
            })
            .unwrap()
            .unwrap()
            .event;
        assert_eq!(
            (
                repeated.down_time,
                repeated.event_time,
                repeated.repeat,
                repeated.serial
            ),
            (100, 300, 2, 55)
        );
        let up = engine.process(key(30, 1, 0)).unwrap().unwrap().event;
        assert_eq!(up.event_time, 300);
    }
    #[test]
    fn retirement_only_cancels_the_selected_app() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.publish(
            Target {
                task: 43,
                ..target()
            },
            1,
        );
        engine.process(key(30, 0, 0)).unwrap();
        engine
            .process(Sample {
                task: 43,
                ..touch(0, 0)
            })
            .unwrap();
        assert_eq!(engine.retire(42, 7, 200).len(), 1);
        assert!(
            engine
                .process(Sample {
                    task: 43,
                    ..touch(0, 1)
                })
                .unwrap()
                .is_some()
        );
        assert!(
            engine
                .process(Sample {
                    display: 1,
                    task: 43,
                    ..touch(0, 1)
                })
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cross_task_contacts_keep_one_display_owner_until_their_physical_up() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.publish(
            Target {
                task: 43,
                ..target()
            },
            1,
        );
        engine.process(touch(0, 0)).unwrap();
        let other = |id, action| Sample {
            task: 43,
            ..touch(id, action)
        };
        // A second app cannot start another DeviceId(-1) stream or contribute
        // coordinates to the first app's pointer set.
        assert!(engine.process(other(1, 0)).unwrap().is_none());
        assert!(engine.process(other(1, 1)).unwrap().is_none());
        let second = engine.process(touch(2, 0)).unwrap().unwrap().event;
        assert_eq!((second.action, second.count), (5 | 1 << 8, 2));
        let cancelled = engine.retire(42, 7, 200);
        assert_eq!(
            (cancelled[0].event.count, cancelled[0].event.flags),
            (2, FLAG_CANCELED)
        );
        assert!(engine.process(other(1, 1)).unwrap().is_none());
        assert!(engine.process(other(1, 2)).unwrap().is_none());
        assert_eq!(
            engine.process(other(1, 0)).unwrap().unwrap().event.action,
            0
        );
        assert_eq!(
            engine.process(other(1, 2)).unwrap().unwrap().event.action,
            1
        );
    }

    #[test]
    fn separate_displays_can_hold_independent_gestures_and_reset_exact_sets() {
        let mut engine = Engine::default();
        engine.publish(target(), 1);
        engine.publish(
            Target {
                task: 43,
                display: 1,
                ..target()
            },
            1,
        );
        engine.process(touch(0, 0)).unwrap();
        engine.process(touch(1, 0)).unwrap();
        let other = Sample {
            task: 43,
            display: 1,
            ..touch(0, 0)
        };
        assert_eq!(engine.process(other).unwrap().unwrap().event.action, 0);
        let cancelled = engine.reset(200);
        assert_eq!(cancelled.len(), 2);
        assert_eq!(cancelled[0].event.count, 2);
        assert_eq!(cancelled[1].event.count, 1);
        assert!(cancelled.iter().all(|e| e.event.flags == FLAG_CANCELED));
        assert_eq!(
            engine.process(touch(0, 0)).unwrap().unwrap().event.action,
            0
        );
    }
}
