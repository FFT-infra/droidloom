//! Lazy connection from the Rust Composer service to Android's framework bridge.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use droidloom_denial_ipc::SeqPacket;
use droidloom_denial_protocol::{InputEvent, TabletAction, TouchAction};
use droidloom_input_protocol::{
    MAX_POINTER_ID, encode_key, encode_tablet, encode_task_bounds, encode_task_close,
    encode_task_focus, encode_touch,
};

#[derive(Default)]
struct BridgeState {
    socket: Option<SeqPacket>,
    pointer_ids: PointerIds,
    tablet_ids: TabletIds,
}

#[derive(Default)]
struct PointerIds {
    routes: BTreeMap<(u32, u32), PointerRoute>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PointerRoute {
    android_id: u32,
    task: u64,
}

impl PointerIds {
    fn translate(
        &mut self,
        display: u32,
        source: u32,
        task: u64,
        action: TouchAction,
    ) -> Result<Option<PointerRoute>, String> {
        let key = (display, source);
        match action {
            TouchAction::Down => {
                if self.routes.contains_key(&key) {
                    return Err(format!(
                        "touch source {source} repeated a live down on display {display}"
                    ));
                }
                let android_id = (0..=MAX_POINTER_ID)
                    .find(|candidate| {
                        !self.routes.iter().any(|((owned_display, _), owned)| {
                            *owned_display == display && owned.android_id == *candidate
                        })
                    })
                    .ok_or_else(|| {
                        format!("Android display {display} exhausted its pointer identities")
                    })?;
                let route = PointerRoute { android_id, task };
                self.routes.insert(key, route);
                Ok(Some(route))
            }
            TouchAction::Motion => self.routes.get(&key).copied().map(Some).ok_or_else(|| {
                format!("touch source {source} has no live down on display {display}")
            }),
            TouchAction::Up => self.routes.remove(&key).map(Some).ok_or_else(|| {
                format!("touch source {source} has no live down on display {display}")
            }),
            TouchAction::Cancel => {
                let android_id = self.routes.get(&key).copied();
                self.routes
                    .retain(|(owned_display, _), _| *owned_display != display);
                Ok(android_id)
            }
        }
    }

    fn clear(&mut self) {
        self.routes.clear();
    }
}

#[derive(Default)]
struct TabletIds {
    routes: BTreeMap<(u32, u32), PointerRoute>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TabletTranslation {
    route: PointerRoute,
    synthesize_proximity: bool,
}

impl TabletIds {
    fn translate(
        &mut self,
        display: u32,
        tool_id: u32,
        task: u64,
        action: TabletAction,
    ) -> Result<Option<TabletTranslation>, String> {
        let key = (display, tool_id);
        if let Some(route) = self.routes.get(&key).copied() {
            if route.task != task {
                return Err(format!(
                    "tablet tool {tool_id} changed task while in proximity on display {display}"
                ));
            }
            if action == TabletAction::ProximityIn {
                return Err(format!(
                    "tablet tool {tool_id} repeated proximity-in on display {display}"
                ));
            }
            if matches!(action, TabletAction::ProximityOut | TabletAction::Cancel) {
                self.routes.remove(&key);
            }
            return Ok(Some(TabletTranslation {
                route,
                synthesize_proximity: false,
            }));
        }

        // The Java bridge clears its state when its socket disconnects. The next
        // sample may arrive while the pen is still in proximity, so recreate the
        // proximity event before forwarding that sample.
        if matches!(
            action,
            TabletAction::Up | TabletAction::ProximityOut | TabletAction::Cancel
        ) {
            return Ok(None);
        }
        let android_id = (0..=MAX_POINTER_ID)
            .find(|candidate| {
                !self.routes.iter().any(|((owned_display, _), owned)| {
                    *owned_display == display && owned.android_id == *candidate
                })
            })
            .ok_or_else(|| format!("Android display {display} exhausted its tablet identities"))?;
        let route = PointerRoute { android_id, task };
        self.routes.insert(key, route);
        Ok(Some(TabletTranslation {
            route,
            synthesize_proximity: action != TabletAction::ProximityIn,
        }))
    }

    fn clear(&mut self) {
        self.routes.clear();
    }
}

/// Reconnecting sender for routed Android input records.
pub(super) struct InputBridge {
    path: PathBuf,
    state: Mutex<BridgeState>,
}

impl InputBridge {
    pub(super) fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            state: Mutex::new(BridgeState::default()),
        }
    }

    pub(super) fn send(
        &self,
        route_serial: u64,
        android_display: u32,
        task: u64,
        scale_numerator: u32,
        scale_denominator: u32,
        timestamp_nanos: u64,
        event: InputEvent,
    ) -> Result<(), String> {
        let tablet_trace = match event {
            InputEvent::Tablet {
                action, tool_id, ..
            } if !matches!(action, TabletAction::Motion | TabletAction::Wheel) => {
                Some((action, tool_id))
            }
            _ => None,
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Android framework bridge state lock is poisoned".to_owned())?;
        let record = match event {
            InputEvent::Touch {
                action,
                pointer_id,
                x_fixed,
                y_fixed,
                pressure,
            } => {
                // Wayland reports surface-local logical coordinates. Android's
                // task bounds use the higher-resolution buffer coordinates, so
                // apply the exact scale advertised with the matching configure.
                let x_fixed = scale_fixed_coordinate(x_fixed, scale_numerator, scale_denominator)?;
                let y_fixed = scale_fixed_coordinate(y_fixed, scale_numerator, scale_denominator)?;
                let Some(route) =
                    state
                        .pointer_ids
                        .translate(android_display, pointer_id, task, action)?
                else {
                    return Ok(());
                };
                encode_touch(
                    android_display,
                    route.task,
                    timestamp_nanos,
                    action as u8,
                    route.android_id,
                    x_fixed,
                    y_fixed,
                    pressure,
                )
            }
            InputEvent::Key {
                action,
                keycode,
                repeat,
            } => encode_key(
                android_display,
                task,
                timestamp_nanos,
                action as u8,
                keycode,
                repeat,
                route_serial,
            ),
            InputEvent::Tablet {
                action,
                tool_id,
                tool_type,
                x_fixed,
                y_fixed,
                pressure,
                distance,
                tilt_x_tenths,
                tilt_y_tenths,
                rotation_tenths,
                wheel_clicks,
                slider,
                wheel_degrees_fixed,
                button,
                axis_flags,
            } => {
                let x_fixed = scale_fixed_coordinate(x_fixed, scale_numerator, scale_denominator)?;
                let y_fixed = scale_fixed_coordinate(y_fixed, scale_numerator, scale_denominator)?;
                let Some(translation) =
                    state
                        .tablet_ids
                        .translate(android_display, tool_id, task, action)?
                else {
                    return Ok(());
                };
                let route = translation.route;
                let record = encode_tablet(
                    android_display,
                    route.task,
                    timestamp_nanos,
                    action as u8,
                    route.android_id,
                    x_fixed,
                    y_fixed,
                    pressure,
                    distance,
                    tilt_x_tenths,
                    tilt_y_tenths,
                    rotation_tenths,
                    wheel_clicks,
                    slider,
                    wheel_degrees_fixed,
                    button,
                    tool_type as u8,
                    axis_flags,
                )
                .map_err(|error| error.to_string())?;
                if translation.synthesize_proximity {
                    let mut proximity = record;
                    proximity[7] = TabletAction::ProximityIn as u8;
                    proximity[52..56].fill(0); // A button belongs to the following event.
                    self.send_record(&mut state, &proximity)?;
                }
                Ok(record)
            }
            InputEvent::Navigation { .. } => {
                return Err("navigation injection is not implemented yet".to_owned());
            }
        };
        let record = record.map_err(|error| error.to_string())?;
        let result = self.send_record(&mut state, &record);
        if result.is_ok() {
            if let Some((action, tool_id)) = tablet_trace {
                eprintln!(
                    "Droidloom tablet trace: stage=framework-bridge-sent action={action:?} tool={tool_id} task={task}"
                );
            }
        }
        result
    }

    /// Ask Android to reflow one freeform task to Denial's content size.
    pub(super) fn send_task_bounds(
        &self,
        task: u64,
        android_display: u32,
        width: u32,
        height: u32,
        scale_numerator: u32,
        scale_denominator: u32,
    ) -> Result<(), String> {
        let record = encode_task_bounds(
            task,
            android_display,
            width,
            height,
            scale_numerator,
            scale_denominator,
        )
        .map_err(|error| error.to_string())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Android framework bridge state lock is poisoned".to_owned())?;
        self.send_record(&mut state, &record)
    }

    /// Mirror the host compositor's keyboard focus into Android's task model.
    pub(super) fn send_task_focus(&self, task: u64, focused: bool) -> Result<(), String> {
        let record = encode_task_focus(task, focused).map_err(|error| error.to_string())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Android framework bridge state lock is poisoned".to_owned())?;
        self.send_record(&mut state, &record)
    }

    /// Ask Android to finish and remove one host-closed task.
    pub(super) fn send_task_close(&self, task: u64) -> Result<(), String> {
        let record = encode_task_close(task).map_err(|error| error.to_string())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Android framework bridge state lock is poisoned".to_owned())?;
        self.send_record(&mut state, &record)
    }

    fn send_record(&self, state: &mut BridgeState, record: &[u8]) -> Result<(), String> {
        if state.socket.is_none() {
            match connect(&self.path) {
                Ok(socket) => state.socket = Some(socket),
                Err(error) => {
                    state.pointer_ids.clear();
                    state.tablet_ids.clear();
                    return Err(error);
                }
            }
        }
        let result = state
            .socket
            .as_ref()
            .expect("framework bridge socket was initialized")
            .send_record(record, &[])
            .map_err(|error| format!("send Android framework record: {error}"));
        if result.is_err() {
            state.socket = None;
            state.pointer_ids.clear();
            state.tablet_ids.clear();
        }
        result
    }
}

fn scale_fixed_coordinate(value: i32, numerator: u32, denominator: u32) -> Result<i32, String> {
    if numerator == 0 || denominator == 0 {
        return Err(format!(
            "invalid logical-to-buffer input scale {numerator}/{denominator}"
        ));
    }
    let product = i128::from(value) * i128::from(numerator);
    let half = i128::from(denominator / 2);
    let rounded = if product >= 0 {
        (product + half) / i128::from(denominator)
    } else {
        (product - half) / i128::from(denominator)
    };
    i32::try_from(rounded).map_err(|_| "scaled input coordinate overflowed".to_owned())
}

fn connect(path: &Path) -> Result<SeqPacket, String> {
    SeqPacket::connect(path).map_err(|error| {
        format!(
            "connect Android framework bridge {}: {error}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_output_scale_maps_logical_input_to_android_pixels() {
        assert_eq!(scale_fixed_coordinate(240 << 16, 132, 120), Ok(264 << 16));
        assert_eq!(scale_fixed_coordinate(400 << 16, 3, 2), Ok(600 << 16));
    }

    #[test]
    fn opaque_host_touch_slots_are_mapped_to_android_ids() {
        let mut ids = PointerIds::default();
        assert_eq!(
            ids.translate(0, i32::MAX as u32, 29, TouchAction::Down),
            Ok(Some(PointerRoute {
                android_id: 0,
                task: 29
            }))
        );
        assert_eq!(
            ids.translate(0, i32::MAX as u32, 99, TouchAction::Motion),
            Ok(Some(PointerRoute {
                android_id: 0,
                task: 29
            }))
        );
        assert_eq!(
            ids.translate(0, i32::MAX as u32, 99, TouchAction::Up),
            Ok(Some(PointerRoute {
                android_id: 0,
                task: 29
            }))
        );
    }

    #[test]
    fn pointer_ids_are_scoped_to_android_display() {
        let mut ids = PointerIds::default();
        assert_eq!(
            ids.translate(0, 8, 29, TouchAction::Down)
                .unwrap()
                .unwrap()
                .android_id,
            0
        );
        assert_eq!(
            ids.translate(0, 9, 29, TouchAction::Down)
                .unwrap()
                .unwrap()
                .android_id,
            1
        );
        assert_eq!(
            ids.translate(4, 8, 29, TouchAction::Down)
                .unwrap()
                .unwrap()
                .android_id,
            0
        );
        assert_eq!(
            ids.translate(0, 8, 29, TouchAction::Up)
                .unwrap()
                .unwrap()
                .android_id,
            0
        );
        assert_eq!(
            ids.translate(0, 10, 29, TouchAction::Down)
                .unwrap()
                .unwrap()
                .android_id,
            0
        );
    }

    #[test]
    fn tablet_routes_recover_after_bridge_disconnect() {
        let mut ids = TabletIds::default();
        let first = ids
            .translate(0, 42, 29, TabletAction::ProximityIn)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.route,
            PointerRoute {
                android_id: 0,
                task: 29
            }
        );
        assert!(!first.synthesize_proximity);
        assert!(
            !ids.translate(0, 42, 29, TabletAction::Down)
                .unwrap()
                .unwrap()
                .synthesize_proximity
        );

        ids.clear();
        let resumed = ids
            .translate(0, 42, 29, TabletAction::Motion)
            .unwrap()
            .unwrap();
        assert_eq!(resumed.route, first.route);
        assert!(resumed.synthesize_proximity);
        assert!(
            !ids.translate(0, 42, 29, TabletAction::Down)
                .unwrap()
                .unwrap()
                .synthesize_proximity
        );
        assert!(ids.translate(0, 42, 99, TabletAction::Motion).is_err());
        assert_eq!(
            ids.translate(0, 42, 29, TabletAction::ProximityOut)
                .unwrap()
                .unwrap()
                .route,
            first.route
        );
        assert_eq!(ids.translate(0, 42, 29, TabletAction::Up).unwrap(), None);
    }

    #[test]
    fn failed_bridge_connect_discards_input_routes() {
        let bridge = InputBridge::new("/dev/null/droidloom-input-bridge");
        let mut state = BridgeState::default();
        state
            .tablet_ids
            .translate(0, 42, 29, TabletAction::ProximityIn)
            .unwrap();
        state
            .pointer_ids
            .translate(0, 7, 29, TouchAction::Down)
            .unwrap();
        assert!(
            bridge
                .send_record(&mut state, &[0; droidloom_input_protocol::RECORD_BYTES])
                .is_err()
        );
        assert!(state.tablet_ids.routes.is_empty());
        assert!(state.pointer_ids.routes.is_empty());
    }

    #[test]
    fn resumed_tablet_down_sends_proximity_before_contact() {
        let (sender, receiver) = SeqPacket::pair().unwrap();
        let bridge = InputBridge::new("/dev/null/droidloom-input-bridge");
        bridge.state.lock().unwrap().socket = Some(sender);
        bridge
            .send(
                1,
                0,
                29,
                1,
                1,
                123_000_000,
                InputEvent::Tablet {
                    action: TabletAction::Down,
                    tool_id: 42,
                    tool_type: droidloom_denial_protocol::TabletToolType::Pen,
                    x_fixed: 100 << 16,
                    y_fixed: 200 << 16,
                    pressure: 30_000,
                    distance: 0,
                    tilt_x_tenths: 0,
                    tilt_y_tenths: 0,
                    rotation_tenths: 0,
                    wheel_clicks: 0,
                    slider: 0,
                    wheel_degrees_fixed: 0,
                    button: 0,
                    axis_flags: droidloom_input_protocol::TABLET_AXIS_PRESSURE,
                },
            )
            .unwrap();
        let proximity = receiver.receive_record().unwrap().bytes;
        let down = receiver.receive_record().unwrap().bytes;
        assert_eq!(proximity.len(), droidloom_input_protocol::RECORD_BYTES);
        assert_eq!(proximity[6], droidloom_input_protocol::KIND_TABLET);
        assert_eq!(proximity[7], TabletAction::ProximityIn as u8);
        assert_eq!(down[7], TabletAction::Down as u8);
        assert_eq!(&proximity[12..16], &down[12..16]);
        assert_eq!(&proximity[24..52], &down[24..52]);
    }
}
