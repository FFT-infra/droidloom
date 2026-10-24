//! Native stylus and tablet protocol handling for Wayland zwp_tablet_v2.

use super::*;
use std::collections::BTreeSet;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, Proxy};
use wayland_protocols::wp::tablet::zv2::client::{
    self as tablet,
    zwp_tablet_manager_v2::ZwpTabletManagerV2,
    zwp_tablet_pad_dial_v2::ZwpTabletPadDialV2,
    zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2,
    zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
    zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
    zwp_tablet_pad_v2::ZwpTabletPadV2,
    zwp_tablet_seat_v2::ZwpTabletSeatV2,
    zwp_tablet_tool_v2::ZwpTabletToolV2,
    zwp_tablet_v2::ZwpTabletV2,
};

macro_rules! tablet_trace {
    ($($arg:tt)*) => {
        if std::env::var_os("DROIDLOOM_TABLET_TRACE").is_some() {
            eprintln!($($arg)*);
        }
    };
}

pub(super) const PAD_BUTTON_BASE: u32 = 0x106;
pub(super) const PAD_BUTTON_COUNT: u32 = 4;

pub(super) fn tilt_degrees(value: f64) -> i8 {
    if value.is_finite() {
        value.round().clamp(-90.0, 90.0) as i8
    } else {
        0
    }
}

pub(super) struct TabletToolState {
    pub(super) tool_id: u32,
    pub(super) surface: Option<wl_surface::WlSurface>,
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) pressure: u16,
    pub(super) is_down: bool,
    pub(super) pending_down: bool,
    pub(super) pending_up: bool,
    pub(super) pending_proximity_out: bool,
    pub(super) dirty_motion: bool,
    pub(super) native_entered: bool,
    pub(super) eraser: bool,
    pub(super) supported: bool,
    pub(super) tilt: (i8, i8),
    pub(super) buttons: u8,
    pub(super) dirty_buttons: bool,
}

impl TabletToolState {
    pub(super) fn new(tool_id: u32) -> Self {
        Self {
            tool_id,
            surface: None,
            x: 0.0,
            y: 0.0,
            pressure: 0,
            is_down: false,
            pending_down: false,
            pending_up: false,
            pending_proximity_out: false,
            dirty_motion: false,
            native_entered: false,
            eraser: false,
            supported: true,
            tilt: (0, 0),
            buttons: 0,
            dirty_buttons: false,
        }
    }
}

pub(super) enum TabletPadChild {
    Group(ZwpTabletPadGroupV2),
    Ring(ZwpTabletPadRingV2),
    Strip(ZwpTabletPadStripV2),
    Dial(ZwpTabletPadDialV2),
}

impl TabletPadChild {
    pub(super) fn destroy(self) {
        match self {
            Self::Group(p) => p.destroy(),
            Self::Ring(p) => p.destroy(),
            Self::Strip(p) => p.destroy(),
            Self::Dial(p) => p.destroy(),
        }
    }
}

pub(super) struct TabletPadState {
    pub(super) tablet_seat_id: u32,
    pub(super) proxy: ZwpTabletPadV2,
    pub(super) children: Vec<TabletPadChild>,
    pub(super) focus: Option<TaskObjectId>,
    pub(super) pressed: BTreeSet<u32>,
}

impl App {
    pub(super) fn bind_tablet_seat(&mut self, seat: wl_seat::WlSeat, qh: &QueueHandle<Self>) {
        if self.tablet_manager.is_none()
            || self
                .tablet_seats
                .iter()
                .any(|(existing, _)| *existing == seat)
        {
            return;
        }
        tablet_trace!("Droidloom tablet: binding tablet seat for {:?}", seat.id());
        let tablet_seat = self
            .tablet_manager
            .as_ref()
            .unwrap()
            .get_tablet_seat(&seat, qh, ());
        self.tablet_seats.push((seat, tablet_seat));
    }

    pub(super) fn unbind_tablet_seat(&mut self, seat: &wl_seat::WlSeat) {
        if let Some(pos) = self.tablet_seats.iter().position(|(s, _)| s == seat) {
            let (_, proxy) = self.tablet_seats.remove(pos);
            let seat_id = proxy.id().protocol_id();
            let pad_ids = self
                .tablet_pads
                .iter()
                .filter_map(|(id, pad)| (pad.tablet_seat_id == seat_id).then_some(*id))
                .collect::<Vec<_>>();
            for pad_id in pad_ids {
                self.release_pad_buttons(pad_id);
                self.destroy_tablet_pad(pad_id);
            }
            proxy.destroy();
        }
    }

    pub(super) fn pad_button(&mut self, pad_id: u32, button: u32, pressed: bool) {
        if button >= PAD_BUTTON_COUNT {
            return;
        }
        let Some(pad) = self.tablet_pads.get_mut(&pad_id) else {
            return;
        };
        let target = pad.focus.or(self.focused);
        let Some(object) = target else { return };
        let changed = if pressed {
            pad.pressed.insert(button)
        } else {
            pad.pressed.remove(&button)
        };
        if !changed {
            return;
        }
        if pressed {
            pad.focus = Some(object);
        }
        tablet_trace!(
            "Droidloom tablet trace: stage=pad-button pad={pad_id} button={button} pressed={pressed} task={}",
            object.0
        );
        let timestamp = monotonic_timestamp_nanos().unwrap_or(0);
        if let Err(error) = self.send_input(
            object,
            InputEvent::Key {
                action: if pressed {
                    KeyAction::Down
                } else {
                    KeyAction::Up
                },
                keycode: PAD_BUTTON_BASE + button,
                repeat: 0,
            },
            timestamp,
        ) {
            self.fail(&error.to_string());
        }
    }

    pub(super) fn release_pad_buttons(&mut self, pad_id: u32) {
        let Some(pad) = self.tablet_pads.get(&pad_id) else {
            return;
        };
        for button in pad.pressed.iter().copied().collect::<Vec<_>>() {
            self.pad_button(pad_id, button, false);
        }
        if let Some(pad) = self.tablet_pads.get_mut(&pad_id) {
            pad.focus = None;
        }
    }

    pub(super) fn destroy_tablet_pad(&mut self, pad_id: u32) {
        if let Some(pad) = self.tablet_pads.remove(&pad_id) {
            for child in pad.children.into_iter().rev() {
                child.destroy();
            }
            pad.proxy.destroy();
        }
        self.tablet_pad_groups.retain(|_, owner| *owner != pad_id);
    }

    pub(super) fn tablet_tool_mut(&mut self, tool_id: u32) -> &mut TabletToolState {
        if let Some(index) = self.tablet_tools.iter().position(|t| t.tool_id == tool_id) {
            &mut self.tablet_tools[index]
        } else {
            self.tablet_tools.push(TabletToolState::new(tool_id));
            self.tablet_tools.last_mut().unwrap()
        }
    }

    pub(super) fn cancel_tablet_tool(&mut self, tool_id: u32) {
        if self.direct_input.supports_mouse() {
            let _ = self.send_native_pen(tool_id, 5);
        }
        let (surface, is_down) = {
            let Some(tool) = self.tablet_tools.iter_mut().find(|t| t.tool_id == tool_id) else {
                return;
            };
            let surface = tool.surface.take();
            let is_down = std::mem::replace(&mut tool.is_down, false);
            (surface, is_down)
        };
        if is_down
            && !self.direct_input.supports_mouse()
            && let Some(surface) = surface
            && let Some(object) = self.task_for_surface(&surface)
        {
            tablet_trace!("Droidloom tablet: cancel_tablet_tool {tool_id} on {object:?}");
            let pointer_id = 0x8000_0000 | tool_id;
            let timestamp = monotonic_timestamp_nanos().unwrap_or(0);
            let _ = self.send_input(
                object,
                InputEvent::Touch {
                    action: TouchAction::Cancel,
                    pointer_id,
                    x_fixed: 0,
                    y_fixed: 0,
                    pressure: 0,
                },
                timestamp,
            );
        }
    }

    pub(super) fn flush_tablet_tool(&mut self, tool_id: u32) {
        if !self.tablet_tool_mut(tool_id).supported {
            return;
        }
        if self.direct_input.supports_mouse() {
            self.flush_native_pen(tool_id);
            return;
        }
        let tool = self.tablet_tool_mut(tool_id);
        let pending_down = std::mem::take(&mut tool.pending_down);
        let pending_up = std::mem::take(&mut tool.pending_up);
        let _pending_out = std::mem::take(&mut tool.pending_proximity_out);
        let _dirty_motion = std::mem::take(&mut tool.dirty_motion);
        let x = tool.x;
        let y = tool.y;
        let pressure = tool.pressure;
        let surface = tool.surface.clone();

        let pointer_id = 0x8000_0000 | tool_id;

        if pending_down {
            if self.tablet_tool_mut(tool_id).is_down {
                if let Some(surface) = &surface
                    && let Some(object) = self.task_for_surface(surface)
                {
                    let (x_fixed, y_fixed) = self.fixed_position(object, (x, y));
                    let timestamp = monotonic_timestamp_nanos().unwrap_or(0);
                    let _ = self.send_input(
                        object,
                        InputEvent::Touch {
                            action: TouchAction::Up,
                            pointer_id,
                            x_fixed,
                            y_fixed,
                            pressure: 0,
                        },
                        timestamp,
                    );
                }
                self.tablet_tool_mut(tool_id).is_down = false;
            }
            if let Some(surface) = &surface
                && let Some(object) = self.task_for_surface(surface)
            {
                self.text_input.note_touch();
                if self.focused != Some(object) {
                    let _ = self.set_focus(Some(object));
                }
                let (x_fixed, y_fixed) = self.fixed_position(object, (x, y));
                let press = if pressure > 0 { pressure } else { u16::MAX };
                tablet_trace!(
                    "Droidloom tablet: send Down to {object:?} (x={x_fixed}, y={y_fixed}, pressure={press})"
                );
                let timestamp = monotonic_timestamp_nanos().unwrap_or(0);
                match self.send_input(
                    object,
                    InputEvent::Touch {
                        action: TouchAction::Down,
                        pointer_id,
                        x_fixed,
                        y_fixed,
                        pressure: press,
                    },
                    timestamp,
                ) {
                    Ok(true) => self.tablet_tool_mut(tool_id).is_down = true,
                    Ok(false) => {}
                    Err(error) => {
                        self.fail(&error.to_string());
                    }
                }
            }
        } else if pending_up {
            if self.tablet_tool_mut(tool_id).is_down {
                if let Some(surface) = &surface
                    && let Some(object) = self.task_for_surface(surface)
                {
                    let (x_fixed, y_fixed) = self.fixed_position(object, (x, y));
                    tablet_trace!("Droidloom tablet: send Up to {object:?}");
                    let timestamp = monotonic_timestamp_nanos().unwrap_or(0);
                    let _ = self.send_input(
                        object,
                        InputEvent::Touch {
                            action: TouchAction::Up,
                            pointer_id,
                            x_fixed,
                            y_fixed,
                            pressure: 0,
                        },
                        timestamp,
                    );
                }
                self.tablet_tool_mut(tool_id).is_down = false;
            }
        }
    }

    pub(super) fn cancel_task_touch_contacts(&mut self, object: TaskObjectId) -> Result<bool, String> {
        let contacts = self
            .touch_contacts
            .values()
            .filter(|contact| contact.object == object)
            .copied()
            .collect::<Vec<_>>();
        if contacts.is_empty() {
            return Ok(true);
        }

        let direct = self
            .endpoint
            .as_ref()
            .is_some_and(droidloom_denial_endpoint::DenialEndpoint::direct_input);
        if direct {
            let timestamp = monotonic_timestamp_nanos().map_err(|error| error.to_string())?;
            for contact in contacts {
                if let Err(error) =
                    self.direct_input
                        .cancel_task_pointer(object.0, contact.pointer_id, timestamp)
                {
                    self.fail(&error);
                    return Err(error);
                }
            }
            return Ok(true);
        }

        let timestamp = monotonic_timestamp_nanos().map_err(|error| error.to_string())?;
        for contact in contacts {
            if let Err(error) = self.send_input(object, contact.event(TouchAction::Cancel), timestamp) {
                self.fail(&error.to_string());
                return Err(error.to_string());
            }
        }
        Ok(true)
    }

    pub(super) fn send_native_pen(&mut self, tool_id: u32, action: u8) -> bool {
        let Some((surface, tool_x, tool_y, tool_pressure, tool_tilt, tool_eraser, tool_buttons)) = self
            .tablet_tools
            .iter()
            .find(|tool| tool.tool_id == tool_id)
            .map(|t| (t.surface.clone(), t.x, t.y, t.pressure, t.tilt, t.eraser, t.buttons))
        else {
            return false;
        };
        let target = surface
            .as_ref()
            .and_then(|s| self.task_for_surface(s))
            .or(self.focused);
        let Some(object) = target else {
            return false;
        };
        let Some((route, scale_120, task_surface)) = self
            .tasks
            .get(&object)
            .filter(|task| task.accepts_present())
            .and_then(|task| {
                task.input_route
                    .map(|route| (route, task.applied_scale_120, task.surface().cloned()))
            })
        else {
            return false;
        };
        let _surface = if let Some(s) = surface {
            s
        } else if let Some(s) = task_surface {
            self.tablet_tool_mut(tool_id).surface = Some(s.clone());
            s
        } else {
            return false;
        };
        let scale = (scale_120, FRACTIONAL_SCALE_DENOMINATOR);
        let fixed = self.fixed_position(object, (tool_x, tool_y));
        let pen = droidloom_input_protocol::Stylus {
            tool: tool_id,
            action,
            x_fixed: fixed.0,
            y_fixed: fixed.1,
            pressure: if matches!(action, 2 | 4 | 5) {
                0
            } else {
                tool_pressure
            },
            tilt_x: tool_tilt.0,
            tilt_y: tool_tilt.1,
            eraser: tool_eraser,
            buttons: tool_buttons,
        };
        let Ok(timestamp) = monotonic_timestamp_nanos() else {
            return false;
        };
        self.direct_input.set_blocked(self.clipboard.blocked());
        match self.direct_input.send_stylus(route, scale, timestamp, pen) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("Droidloom native pen: {error}");
                self.direct_input.disconnect();
                self.retire_contacts(None);
                false
            }
        }
    }

    pub(super) fn flush_native_pen(&mut self, tool_id: u32) {
        if self.tablet_tool_mut(tool_id).pending_down {
            self.text_input.note_touch();
            let surface = self.tablet_tool_mut(tool_id).surface.clone();
            if let Some(surface) = surface
                && let Some(object) = self.task_for_surface(&surface)
            {
                let _ = self.cancel_task_touch_contacts(object);
            }
        }

        let tool = self.tablet_tool_mut(tool_id);
        let down = std::mem::take(&mut tool.pending_down);
        let up = std::mem::take(&mut tool.pending_up);
        let out = std::mem::take(&mut tool.pending_proximity_out);
        let motion = std::mem::take(&mut tool.dirty_motion);
        let buttons = std::mem::take(&mut tool.dirty_buttons);
        if !tool.native_entered {
            if self.send_native_pen(tool_id, 0) {
                self.tablet_tool_mut(tool_id).native_entered = true;
            } else {
                if out {
                    self.tablet_tool_mut(tool_id).surface = None;
                }
                return;
            }
        }
        if down {
            if self.send_native_pen(tool_id, 3) {
                self.tablet_tool_mut(tool_id).is_down = true;
            }
        } else if motion && !up && !out {
            self.send_native_pen(tool_id, 1);
        }
        if buttons {
            self.send_native_pen(tool_id, 6);
        }
        if up {
            self.send_native_pen(tool_id, 4);
            self.tablet_tool_mut(tool_id).is_down = false;
        }
        if out {
            self.send_native_pen(tool_id, 2);
            let tool = self.tablet_tool_mut(tool_id);
            tool.surface = None;
            tool.native_entered = false;
            tool.is_down = false;
            tool.buttons = 0;
        }
    }
}

// Dispatch implementations
impl Dispatch<ZwpTabletManagerV2, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTabletManagerV2,
        _event: tablet::zwp_tablet_manager_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTabletSeatV2, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwpTabletSeatV2,
        event: tablet::zwp_tablet_seat_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let seat_id = proxy.id().protocol_id();
        match &event {
            tablet::zwp_tablet_seat_v2::Event::TabletAdded { id } => {
                tablet_trace!("Droidloom tablet: TabletAdded id={:?}", id.id());
                state
                    .tablet_proxies
                    .insert(id.id().protocol_id(), (seat_id, id.clone()));
            }
            tablet::zwp_tablet_seat_v2::Event::ToolAdded { id } => {
                tablet_trace!("Droidloom tablet: ToolAdded id={:?}", id.id());
                let tool_id = id.id().protocol_id();
                state.tablet_tool_proxies.insert(tool_id, id.clone());
            }
            tablet::zwp_tablet_seat_v2::Event::PadAdded { id } => {
                let pad_id = id.id().protocol_id();
                tablet_trace!("Droidloom tablet: PadAdded id={pad_id}");
                state.tablet_pads.insert(
                    pad_id,
                    TabletPadState {
                        tablet_seat_id: seat_id,
                        proxy: id.clone(),
                        children: Vec::new(),
                        focus: None,
                        pressed: BTreeSet::new(),
                    },
                );
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(
        App,
        ZwpTabletSeatV2,
        [
            0 => (ZwpTabletV2, ()),
            1 => (ZwpTabletToolV2, ()),
            2 => (ZwpTabletPadV2, ())
        ]
    );
}

impl Dispatch<ZwpTabletToolV2, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwpTabletToolV2,
        event: tablet::zwp_tablet_tool_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let tool_id = proxy.id().protocol_id();
        match event {
            tablet::zwp_tablet_tool_v2::Event::Type { tool_type } => {
                let tool = state.tablet_tool_mut(tool_id);
                tool.eraser = matches!(
                    tool_type,
                    WEnum::Value(tablet::zwp_tablet_tool_v2::Type::Eraser)
                );
                tool.supported = matches!(
                    tool_type,
                    WEnum::Value(
                        tablet::zwp_tablet_tool_v2::Type::Pen
                            | tablet::zwp_tablet_tool_v2::Type::Eraser
                            | tablet::zwp_tablet_tool_v2::Type::Brush
                            | tablet::zwp_tablet_tool_v2::Type::Pencil
                            | tablet::zwp_tablet_tool_v2::Type::Airbrush
                    )
                );
            }
            tablet::zwp_tablet_tool_v2::Event::Tilt { tilt_x, tilt_y } => {
                let tool = state.tablet_tool_mut(tool_id);
                tool.tilt = (tilt_degrees(tilt_x), tilt_degrees(tilt_y));
                tool.dirty_motion = true;
            }
            tablet::zwp_tablet_tool_v2::Event::ProximityIn { surface, .. } => {
                if state.direct_input.supports_mouse() {
                    state.cancel_tablet_tool(tool_id);
                }
                if let Some(object) = state.task_for_surface(&surface) {
                    let _ = state.cancel_task_touch_contacts(object);
                }
                let prev_surface = state.tablet_tool_mut(tool_id).surface.take();
                let was_down = state.tablet_tool_mut(tool_id).is_down;
                if was_down
                    && let Some(prev) = prev_surface
                    && let Some(object) = state.task_for_surface(&prev)
                {
                    let pointer_id = 0x8000_0000 | tool_id;
                    let timestamp = monotonic_timestamp_nanos().unwrap_or(0);
                    let _ = state.send_input(
                        object,
                        InputEvent::Touch {
                            action: TouchAction::Cancel,
                            pointer_id,
                            x_fixed: 0,
                            y_fixed: 0,
                            pressure: 0,
                        },
                        timestamp,
                    );
                }
                tablet_trace!(
                    "Droidloom tablet: ProximityIn tool_id={tool_id}, surface={:?}, task={:?}",
                    surface.id(),
                    state.task_for_surface(&surface)
                );
                let tool = state.tablet_tool_mut(tool_id);
                tool.is_down = false;
                tool.surface = Some(surface);
                tool.pressure = 0;
                tool.native_entered = false;
                tool.buttons = 0;
                tool.tilt = (0, 0);
                tool.pending_down = false;
                tool.pending_up = false;
                tool.pending_proximity_out = false;
            }
            tablet::zwp_tablet_tool_v2::Event::ProximityOut => {
                tablet_trace!("Droidloom tablet: ProximityOut tool_id={tool_id}");
                state.tablet_tool_mut(tool_id).pending_proximity_out = true;
            }
            tablet::zwp_tablet_tool_v2::Event::Down { .. } => {
                tablet_trace!("Droidloom tablet: Down tool_id={tool_id}");
                state.tablet_tool_mut(tool_id).pending_down = true;
            }
            tablet::zwp_tablet_tool_v2::Event::Up => {
                tablet_trace!("Droidloom tablet: Up tool_id={tool_id}");
                state.tablet_tool_mut(tool_id).pending_up = true;
            }
            tablet::zwp_tablet_tool_v2::Event::Motion { x, y } => {
                let tool = state.tablet_tool_mut(tool_id);
                tool.x = x;
                tool.y = y;
                tool.dirty_motion = true;
            }
            tablet::zwp_tablet_tool_v2::Event::Pressure { pressure } => {
                let tool = state.tablet_tool_mut(tool_id);
                tool.pressure =
                    u16::try_from(pressure.min(u32::from(u16::MAX))).unwrap_or(u16::MAX);
                tool.dirty_motion = true;
            }
            tablet::zwp_tablet_tool_v2::Event::Button {
                button,
                state: button_state,
                ..
            } => {
                if state.direct_input.supports_mouse() && matches!(button, 0x14b | 0x14c) {
                    let bit = if button == 0x14b { 1 } else { 2 };
                    let fallback_surface = state
                        .tablet_tools
                        .iter()
                        .find(|t| t.tool_id == tool_id)
                        .and_then(|t| t.surface.clone())
                        .or_else(|| {
                            state
                                .focused
                                .and_then(|f| state.tasks.get(&f))
                                .and_then(|t| t.surface().cloned())
                        });
                    let tool = state.tablet_tool_mut(tool_id);
                    match button_state {
                        WEnum::Value(tablet::zwp_tablet_tool_v2::ButtonState::Pressed) => {
                            tool.buttons |= bit
                        }
                        WEnum::Value(tablet::zwp_tablet_tool_v2::ButtonState::Released) => {
                            tool.buttons &= !bit
                        }
                        _ => return,
                    }
                    tool.dirty_buttons = true;
                    if tool.surface.is_none() {
                        tool.surface = fallback_surface;
                    }
                    state.flush_tablet_tool(tool_id);
                }
                let action = match button_state {
                    WEnum::Value(tablet::zwp_tablet_tool_v2::ButtonState::Pressed) => {
                        KeyAction::Down
                    }
                    WEnum::Value(tablet::zwp_tablet_tool_v2::ButtonState::Released) => {
                        KeyAction::Up
                    }
                    _ => return,
                };
                let surface = state
                    .tablet_tools
                    .iter()
                    .find(|t| t.tool_id == tool_id)
                    .and_then(|t| t.surface.clone());
                let target_object = surface
                    .as_ref()
                    .and_then(|s| state.task_for_surface(s))
                    .or(state.focused);
                if let Some(object) = target_object {
                    tablet_trace!(
                        "Droidloom tablet: Button tool_id={tool_id}, button={button:#x}, action={action:?}, target={object:?}"
                    );
                    let timestamp = monotonic_timestamp_nanos().unwrap_or(0);
                    let _ = state.send_input(
                        object,
                        InputEvent::Key {
                            action,
                            keycode: button,
                            repeat: 0,
                        },
                        timestamp,
                    );
                }
            }
            tablet::zwp_tablet_tool_v2::Event::Frame { .. } => {
                state.flush_tablet_tool(tool_id);
            }
            tablet::zwp_tablet_tool_v2::Event::Removed => {
                tablet_trace!("Droidloom tablet: Removed tool_id={tool_id}");
                state.cancel_tablet_tool(tool_id);
                state.tablet_tools.retain(|tool| tool.tool_id != tool_id);
                state.tablet_tool_proxies.remove(&tool_id);
                proxy.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpTabletV2, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTabletV2,
        _event: tablet::zwp_tablet_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTabletPadV2, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwpTabletPadV2,
        event: tablet::zwp_tablet_pad_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let pad_id = proxy.id().protocol_id();
        match event {
            tablet::zwp_tablet_pad_v2::Event::Group { pad_group } => {
                let group_id = pad_group.id().protocol_id();
                state.tablet_pad_groups.insert(group_id, pad_id);
                if let Some(pad) = state.tablet_pads.get_mut(&pad_id) {
                    pad.children.push(TabletPadChild::Group(pad_group));
                }
            }
            tablet::zwp_tablet_pad_v2::Event::Enter { surface, .. } => {
                let object = state.task_for_surface(&surface);
                if let Some(pad) = state.tablet_pads.get_mut(&pad_id) {
                    pad.focus = object;
                }
            }
            tablet::zwp_tablet_pad_v2::Event::Leave { .. } => {
                state.release_pad_buttons(pad_id);
            }
            tablet::zwp_tablet_pad_v2::Event::Button {
                button,
                state: button_state,
                ..
            } => {
                let pressed = match button_state {
                    WEnum::Value(tablet::zwp_tablet_pad_v2::ButtonState::Pressed) => true,
                    WEnum::Value(tablet::zwp_tablet_pad_v2::ButtonState::Released) => false,
                    _ => return,
                };
                state.pad_button(pad_id, button, pressed);
            }
            tablet::zwp_tablet_pad_v2::Event::Removed => {
                state.release_pad_buttons(pad_id);
                state.destroy_tablet_pad(pad_id);
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(
        App,
        ZwpTabletPadV2,
        [
            0 => (ZwpTabletPadGroupV2, ())
        ]
    );
}

impl Dispatch<ZwpTabletPadGroupV2, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwpTabletPadGroupV2,
        event: tablet::zwp_tablet_pad_group_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let group_id = proxy.id().protocol_id();
        let Some(pad_id) = state.tablet_pad_groups.get(&group_id).copied() else {
            return;
        };
        let child = match event {
            tablet::zwp_tablet_pad_group_v2::Event::Ring { ring } => {
                Some(TabletPadChild::Ring(ring))
            }
            tablet::zwp_tablet_pad_group_v2::Event::Strip { strip } => {
                Some(TabletPadChild::Strip(strip))
            }
            tablet::zwp_tablet_pad_group_v2::Event::Dial { dial } => {
                Some(TabletPadChild::Dial(dial))
            }
            _ => None,
        };
        if let Some(child) = child
            && let Some(pad) = state.tablet_pads.get_mut(&pad_id)
        {
            pad.children.push(child);
        }
    }

    wayland_client::event_created_child!(
        App,
        ZwpTabletPadGroupV2,
        [1 => (ZwpTabletPadRingV2, ()), 2 => (ZwpTabletPadStripV2, ()), 6 => (ZwpTabletPadDialV2, ())]
    );
}

impl Dispatch<ZwpTabletPadRingV2, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTabletPadRingV2,
        _event: tablet::zwp_tablet_pad_ring_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTabletPadStripV2, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTabletPadStripV2,
        _event: tablet::zwp_tablet_pad_strip_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTabletPadDialV2, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTabletPadDialV2,
        _event: tablet::zwp_tablet_pad_dial_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::tilt_degrees;

    #[test]
    fn tablet_tilt_is_bounded_before_wire_encoding() {
        assert_eq!(tilt_degrees(30.4), 30);
        assert_eq!(tilt_degrees(-45.8), -46);
        assert_eq!(tilt_degrees(150.0), 90);
        assert_eq!(tilt_degrees(f64::NAN), 0);
    }
}
