//! Lazy connection from the Rust Composer service to Android's framework bridge.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::mpsc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use droidloom_denial_ipc::SeqPacket;
use droidloom_denial_protocol::{InputEvent, MouseAction, TabletAction, TouchAction};
use droidloom_input_protocol::{
    MAX_POINTER_ID, encode_key, encode_mouse, encode_tablet, encode_task_bounds,
    encode_task_close, encode_task_focus, encode_touch,
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

/// Maximum number of framework-bound input records buffered away from the
/// compositor event loop.
const INPUT_QUEUE_CAPACITY: usize = 16_384;
/// Small independent lane for window lifecycle commands. It is serviced ahead
/// of queued pointer motion so a slow Android reader cannot freeze graph control.
const CONTROL_QUEUE_CAPACITY: usize = 256;
/// Keep room for key and contact transitions when a broken peer backs up input.
const CRITICAL_INPUT_RESERVE: usize = 512;
/// A blocked framework reader must not retain the writer thread indefinitely.
const FRAMEWORK_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

enum BridgeCommand {
    Input {
        route_serial: u64,
        android_display: u32,
        task: u64,
        scale_numerator: u32,
        scale_denominator: u32,
        timestamp_nanos: u64,
        event: InputEvent,
    },
    TaskBounds {
        task: u64,
        android_display: u32,
        width: u32,
        height: u32,
        scale_numerator: u32,
        scale_denominator: u32,
    },
    TaskFocus {
        task: u64,
        focused: bool,
    },
    TaskClose {
        task: u64,
    },
}

/// Ordered, bounded handoff to a dedicated Android framework socket writer.
pub(super) struct InputBridge {
    queue: Arc<CommandQueue>,
}

impl InputBridge {
    pub(super) fn new(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let queue = Arc::new(CommandQueue::new(
            INPUT_QUEUE_CAPACITY,
            CONTROL_QUEUE_CAPACITY,
        ));
        thread::Builder::new()
            .name("droidloom-android-input-writer".to_owned())
            .spawn({
                let queue = Arc::clone(&queue);
                move || input_writer(path, queue)
            })
            .map_err(|error| format!("start Android input writer: {error}"))?;
        Ok(Self { queue })
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
        self.enqueue(BridgeCommand::Input {
            route_serial,
            android_display,
            task,
            scale_numerator,
            scale_denominator,
            timestamp_nanos,
            event,
        })
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
        self.enqueue(BridgeCommand::TaskBounds {
            task,
            android_display,
            width,
            height,
            scale_numerator,
            scale_denominator,
        })
    }

    /// Mirror the host compositor's keyboard focus into Android's task model.
    pub(super) fn send_task_focus(&self, task: u64, focused: bool) -> Result<(), String> {
        self.enqueue(BridgeCommand::TaskFocus { task, focused })
    }

    /// Ask Android to finish and remove one host-closed task.
    pub(super) fn send_task_close(&self, task: u64) -> Result<(), String> {
        self.enqueue(BridgeCommand::TaskClose { task })
    }

    fn enqueue(&self, command: BridgeCommand) -> Result<(), String> {
        self.queue.push(command)
    }

    #[cfg(test)]
    fn with_socket_for_test(
        socket: SeqPacket,
        input_capacity: usize,
        control_capacity: usize,
        first_command_ready: Sender<()>,
    ) -> Self {
        let _ = socket.set_send_timeout(Some(FRAMEWORK_SEND_TIMEOUT));
        let queue = Arc::new(CommandQueue::new(input_capacity, control_capacity));
        let writer_queue = Arc::clone(&queue);
        thread::spawn(move || {
            let state = BridgeState {
                socket: Some(socket),
                ..BridgeState::default()
            };
            input_writer_with_state(
                PathBuf::from("/unused"),
                writer_queue,
                state,
                Some(first_command_ready),
            );
        });
        Self { queue }
    }

    #[cfg(test)]
    fn with_capacity_for_test(input_capacity: usize, control_capacity: usize) -> Self {
        Self {
            queue: Arc::new(CommandQueue::new(input_capacity, control_capacity)),
        }
    }
}

impl Drop for InputBridge {
    fn drop(&mut self) {
        self.queue.close();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MotionStream {
    Touch {
        display: u32,
        task: u64,
        pointer_id: u32,
    },
    Tablet {
        display: u32,
        task: u64,
        tool_id: u32,
    },
    Mouse {
        display: u32,
        task: u64,
    },
}

struct CommandQueues {
    controls: VecDeque<BridgeCommand>,
    inputs: VecDeque<BridgeCommand>,
    closed: bool,
}

struct CommandQueue {
    queues: Mutex<CommandQueues>,
    ready: Condvar,
    input_capacity: usize,
    control_capacity: usize,
}

impl CommandQueue {
    fn new(input_capacity: usize, control_capacity: usize) -> Self {
        Self {
            queues: Mutex::new(CommandQueues {
                controls: VecDeque::new(),
                inputs: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
            input_capacity,
            control_capacity,
        }
    }

    fn push(&self, command: BridgeCommand) -> Result<(), String> {
        let mut queues = self
            .queues
            .lock()
            .map_err(|_| "Android framework input queue is poisoned".to_owned())?;
        if queues.closed {
            return Err("Android framework input writer has stopped".to_owned());
        }

        if is_control(&command) {
            if let BridgeCommand::TaskClose { task } = &command {
                queues
                    .inputs
                    .retain(|queued| !command_targets_task(queued, *task));
            }
            enqueue_control(&mut queues.controls, command, self.control_capacity)?;
        } else {
            enqueue_input(
                &mut queues.inputs,
                command,
                self.input_capacity,
                CRITICAL_INPUT_RESERVE.min(self.input_capacity / 4),
            )?;
        }
        drop(queues);
        self.ready.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<BridgeCommand> {
        let mut queues = self.queues.lock().ok()?;
        loop {
            if let Some(command) = queues.controls.pop_front() {
                return Some(command);
            }
            if let Some(command) = queues.inputs.pop_front() {
                return Some(command);
            }
            if queues.closed {
                return None;
            }
            queues = self.ready.wait(queues).ok()?;
        }
    }

    #[cfg(test)]
    fn try_pop(&self) -> Option<BridgeCommand> {
        let mut queues = self.queues.lock().ok()?;
        queues
            .controls
            .pop_front()
            .or_else(|| queues.inputs.pop_front())
    }

    fn close(&self) {
        if let Ok(mut queues) = self.queues.lock() {
            queues.closed = true;
            self.ready.notify_all();
        }
    }
}

fn is_control(command: &BridgeCommand) -> bool {
    matches!(
        command,
        BridgeCommand::TaskBounds { .. }
            | BridgeCommand::TaskFocus { .. }
            | BridgeCommand::TaskClose { .. }
    )
}

fn enqueue_control(
    controls: &mut VecDeque<BridgeCommand>,
    command: BridgeCommand,
    capacity: usize,
) -> Result<(), String> {
    if let BridgeCommand::TaskClose { task } = &command {
        let task = *task;
        controls.retain(|queued| !command_targets_task(queued, task));
        if controls.len() >= capacity {
            return Err("Android framework control queue is full".to_owned());
        }
        controls.push_back(BridgeCommand::TaskClose { task });
        return Ok(());
    }

    if let Some(index) = controls
        .iter()
        .rposition(|queued| same_state_control(queued, &command))
    {
        controls.remove(index);
        controls.push_back(command);
        return Ok(());
    }
    if controls.len() >= capacity {
        return Err("Android framework control queue is full".to_owned());
    }
    controls.push_back(command);
    Ok(())
}

fn command_targets_task(command: &BridgeCommand, task: u64) -> bool {
    match command {
        BridgeCommand::TaskBounds { task: queued, .. }
        | BridgeCommand::TaskFocus { task: queued, .. }
        | BridgeCommand::TaskClose { task: queued } => *queued == task,
        BridgeCommand::Input { task: queued, .. } => *queued == task,
    }
}

fn same_state_control(left: &BridgeCommand, right: &BridgeCommand) -> bool {
    match (left, right) {
        (
            BridgeCommand::TaskBounds {
                task: left_task,
                android_display: left_display,
                ..
            },
            BridgeCommand::TaskBounds {
                task: right_task,
                android_display: right_display,
                ..
            },
        ) => left_task == right_task && left_display == right_display,
        (
            BridgeCommand::TaskFocus {
                task: left_task, ..
            },
            BridgeCommand::TaskFocus {
                task: right_task, ..
            },
        ) => left_task == right_task,
        _ => false,
    }
}

fn motion_stream(command: &BridgeCommand) -> Option<MotionStream> {
    let BridgeCommand::Input {
        android_display,
        task,
        event,
        ..
    } = command
    else {
        return None;
    };
    match event {
        InputEvent::Touch {
            action: TouchAction::Motion,
            pointer_id,
            ..
        } => Some(MotionStream::Touch {
            display: *android_display,
            task: *task,
            pointer_id: *pointer_id,
        }),
        InputEvent::Tablet {
            action: TabletAction::Motion,
            tool_id,
            ..
        } => Some(MotionStream::Tablet {
            display: *android_display,
            task: *task,
            tool_id: *tool_id,
        }),
        InputEvent::Mouse {
            action: MouseAction::Motion,
            ..
        } => Some(MotionStream::Mouse {
            display: *android_display,
            task: *task,
        }),
        _ => None,
    }
}

fn is_stream_boundary(command: &BridgeCommand, stream: MotionStream) -> bool {
    let BridgeCommand::Input {
        android_display,
        task,
        event,
        ..
    } = command
    else {
        return false;
    };
    match (stream, event) {
        (
            MotionStream::Touch {
                display,
                task: stream_task,
                pointer_id,
            },
            InputEvent::Touch {
                pointer_id: queued_id,
                action,
                ..
            },
        ) if display == *android_display && stream_task == *task && pointer_id == *queued_id => {
            *action != TouchAction::Motion
        }
        (
            MotionStream::Tablet {
                display,
                task: stream_task,
                tool_id,
            },
            InputEvent::Tablet {
                tool_id: queued_id,
                action,
                ..
            },
        ) if display == *android_display && stream_task == *task && tool_id == *queued_id => {
            *action != TabletAction::Motion
        }
        (
            MotionStream::Mouse {
                display,
                task: stream_task,
            },
            InputEvent::Mouse { action, .. },
        ) if display == *android_display && stream_task == *task => *action != MouseAction::Motion,
        _ => false,
    }
}

fn enqueue_input(
    inputs: &mut VecDeque<BridgeCommand>,
    command: BridgeCommand,
    capacity: usize,
    critical_reserve: usize,
) -> Result<(), String> {
    let motion = motion_stream(&command);
    let coalesce_at = capacity.saturating_sub(critical_reserve);
    if motion.is_some() && inputs.len() >= coalesce_at {
        if replace_pending_motion(inputs, command, motion.expect("motion stream was checked")) {
            return Ok(());
        }
        // Under pressure, stale pure motion is expendable. Contact, key and
        // button transitions retain the reserved queue space.
        return Ok(());
    }
    if inputs.len() >= capacity {
        if let Some(stream) = motion {
            let _ = replace_pending_motion(inputs, command, stream);
            return Ok(());
        }
        inputs.retain(|queued| motion_stream(queued).is_none());
        if inputs.len() < capacity {
            inputs.push_back(command);
            return Ok(());
        }
        return Err("Android framework input queue is full".to_owned());
    }
    inputs.push_back(command);
    Ok(())
}

fn replace_pending_motion(
    inputs: &mut VecDeque<BridgeCommand>,
    command: BridgeCommand,
    stream: MotionStream,
) -> bool {
    for index in (0..inputs.len()).rev() {
        let queued = &inputs[index];
        if motion_stream(queued) == Some(stream) {
            inputs.remove(index);
            inputs.push_back(command);
            return true;
        }
        if is_stream_boundary(queued, stream) {
            return false;
        }
    }
    false
}

fn input_writer(path: PathBuf, queue: Arc<CommandQueue>) {
    input_writer_with_state(path, queue, BridgeState::default(), None);
}

fn input_writer_with_state(
    path: PathBuf,
    queue: Arc<CommandQueue>,
    mut state: BridgeState,
    mut first_command_ready: Option<Sender<()>>,
) {
    while let Some(command) = queue.pop() {
        if let Some(ready) = first_command_ready.take() {
            let _ = ready.send(());
        }
        let result = match command {
            BridgeCommand::Input {
                route_serial,
                android_display,
                task,
                scale_numerator,
                scale_denominator,
                timestamp_nanos,
                event,
            } => state.send(
                &path,
                route_serial,
                android_display,
                task,
                scale_numerator,
                scale_denominator,
                timestamp_nanos,
                event,
            ),
            BridgeCommand::TaskBounds {
                task,
                android_display,
                width,
                height,
                scale_numerator,
                scale_denominator,
            } => state.send_task_bounds(
                &path,
                task,
                android_display,
                width,
                height,
                scale_numerator,
                scale_denominator,
            ),
            BridgeCommand::TaskFocus { task, focused } => {
                state.send_task_focus(&path, task, focused)
            }
            BridgeCommand::TaskClose { task } => state.send_task_close(&path, task),
        };
        if let Err(error) = result {
            eprintln!("Droidloom Android framework bridge send failed: {error}");
        }
    }
}

impl BridgeState {
    fn send(
        &mut self,
        path: &Path,
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
                    self.pointer_ids
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
                    self.tablet_ids
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
                    self.send_record(path, &proximity)?;
                }
                Ok(record)
            }
            InputEvent::Mouse {
                action,
                x_fixed,
                y_fixed,
                button,
                scroll_x_fixed,
                scroll_y_fixed,
            } => {
                let x_fixed = scale_fixed_coordinate(x_fixed, scale_numerator, scale_denominator)?;
                let y_fixed = scale_fixed_coordinate(y_fixed, scale_numerator, scale_denominator)?;
                encode_mouse(
                    android_display,
                    task,
                    timestamp_nanos,
                    action as u8,
                    x_fixed,
                    y_fixed,
                    button,
                    scroll_x_fixed,
                    scroll_y_fixed,
                )
            }
            InputEvent::Navigation { .. } => {
                return Err("navigation injection is not implemented yet".to_owned());
            }
        };
        let record = record.map_err(|error| error.to_string())?;
        let result = self.send_record(path, &record);
        if result.is_ok() {
            if let Some((action, tool_id)) = tablet_trace {
                eprintln!(
                    "Droidloom tablet trace: stage=framework-bridge-sent action={action:?} tool={tool_id} task={task}"
                );
            }
        }
        result
    }

    fn send_task_bounds(
        &mut self,
        path: &Path,
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
        self.send_record(path, &record)
    }

    fn send_task_focus(&mut self, path: &Path, task: u64, focused: bool) -> Result<(), String> {
        let record = encode_task_focus(task, focused).map_err(|error| error.to_string())?;
        self.send_record(path, &record)
    }

    fn send_task_close(&mut self, path: &Path, task: u64) -> Result<(), String> {
        let record = encode_task_close(task).map_err(|error| error.to_string())?;
        self.send_record(path, &record)
    }

    fn send_record(&mut self, path: &Path, record: &[u8]) -> Result<(), String> {
        if self.socket.is_none() {
            match connect(path) {
                Ok(socket) => self.socket = Some(socket),
                Err(error) => {
                    self.pointer_ids.clear();
                    self.tablet_ids.clear();
                    return Err(error);
                }
            }
        }
        let result = self
            .socket
            .as_ref()
            .expect("framework bridge socket was initialized")
            .send_record(record, &[])
            .map_err(|error| format!("send Android framework record: {error}"));
        if result.is_err() {
            self.socket = None;
            self.pointer_ids.clear();
            self.tablet_ids.clear();
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
    let socket = SeqPacket::connect(path).map_err(|error| {
        format!(
            "connect Android framework bridge {}: {error}",
            path.display()
        )
    })?;
    socket
        .set_send_timeout(Some(FRAMEWORK_SEND_TIMEOUT))
        .map_err(|error| format!("set Android framework bridge send timeout: {error}"))?;
    Ok(socket)
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
            state
                .send_record(
                    Path::new("/dev/null/droidloom-input-bridge"),
                    &[0; droidloom_input_protocol::RECORD_BYTES]
                )
                .is_err()
        );
        assert!(state.tablet_ids.routes.is_empty());
        assert!(state.pointer_ids.routes.is_empty());
    }

    #[test]
    fn bounded_input_handoff_coalesces_bounds_and_never_blocks_on_full_lanes() {
        let bridge = InputBridge::with_capacity_for_test(2, 1);
        bridge.send_task_bounds(7, 0, 640, 480, 1, 1).unwrap();
        bridge
            .send(
                1,
                0,
                7,
                1,
                1,
                10,
                InputEvent::Touch {
                    action: TouchAction::Down,
                    pointer_id: 9,
                    x_fixed: 100 << 16,
                    y_fixed: 200 << 16,
                    pressure: 40_000,
                },
            )
            .unwrap();
        bridge
            .send(
                2,
                0,
                7,
                1,
                1,
                11,
                InputEvent::Touch {
                    action: TouchAction::Up,
                    pointer_id: 9,
                    x_fixed: 100 << 16,
                    y_fixed: 200 << 16,
                    pressure: 0,
                },
            )
            .unwrap();
        assert!(
            bridge
                .send(
                    3,
                    0,
                    7,
                    1,
                    1,
                    12,
                    InputEvent::Key {
                        action: droidloom_denial_protocol::KeyAction::Down,
                        keycode: 30,
                        repeat: 0,
                    },
                )
                .is_err()
        );
        bridge.send_task_bounds(7, 0, 800, 600, 1, 1).unwrap();
        assert!(bridge.send_task_focus(7, true).is_err());

        assert!(matches!(
            bridge.queue.try_pop().unwrap(),
            BridgeCommand::TaskBounds {
                task: 7,
                android_display: 0,
                width: 800,
                height: 600,
                ..
            }
        ));
        assert!(matches!(
            bridge.queue.try_pop().unwrap(),
            BridgeCommand::Input {
                route_serial: 1,
                event: InputEvent::Touch {
                    action: TouchAction::Down,
                    pointer_id: 9,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            bridge.queue.try_pop().unwrap(),
            BridgeCommand::Input {
                route_serial: 2,
                event: InputEvent::Touch {
                    action: TouchAction::Up,
                    pointer_id: 9,
                    ..
                },
                ..
            }
        ));
        assert!(bridge.queue.try_pop().is_none());
    }

    #[test]
    fn closing_a_task_discards_its_queued_input_and_supersedes_controls() {
        let bridge = InputBridge::with_capacity_for_test(4, 2);
        bridge.send_task_bounds(7, 0, 640, 480, 1, 1).unwrap();
        bridge
            .send(
                1,
                0,
                7,
                1,
                1,
                10,
                InputEvent::Touch {
                    action: TouchAction::Down,
                    pointer_id: 9,
                    x_fixed: 100 << 16,
                    y_fixed: 200 << 16,
                    pressure: 40_000,
                },
            )
            .unwrap();
        bridge.send_task_close(7).unwrap();
        assert!(matches!(
            bridge.queue.try_pop(),
            Some(BridgeCommand::TaskClose { task: 7 })
        ));
        assert!(bridge.queue.try_pop().is_none());
    }

    #[test]
    fn pressure_coalesces_motion_without_crossing_contact_transitions() {
        let bridge = InputBridge::with_capacity_for_test(8, 1);
        bridge
            .send(
                1,
                0,
                7,
                1,
                1,
                1,
                InputEvent::Touch {
                    action: TouchAction::Motion,
                    pointer_id: 1,
                    x_fixed: 100 << 16,
                    y_fixed: 100 << 16,
                    pressure: 1,
                },
            )
            .unwrap();
        for pointer_id in 2..=6 {
            bridge
                .send(
                    1,
                    0,
                    7,
                    1,
                    1,
                    u64::from(pointer_id),
                    InputEvent::Touch {
                        action: TouchAction::Down,
                        pointer_id,
                        x_fixed: 0,
                        y_fixed: 0,
                        pressure: 1,
                    },
                )
                .unwrap();
        }
        bridge
            .send(
                2,
                0,
                7,
                1,
                1,
                99,
                InputEvent::Touch {
                    action: TouchAction::Motion,
                    pointer_id: 1,
                    x_fixed: 900 << 16,
                    y_fixed: 700 << 16,
                    pressure: 2,
                },
            )
            .unwrap();

        let mut queued = Vec::new();
        while let Some(command) = bridge.queue.try_pop() {
            queued.push(command);
        }
        assert_eq!(queued.len(), 6);
        assert!(matches!(
            queued.last(),
            Some(BridgeCommand::Input {
                route_serial: 2,
                timestamp_nanos: 99,
                event: InputEvent::Touch {
                    action: TouchAction::Motion,
                    ..
                },
                ..
            })
        ));
        let Some(BridgeCommand::Input {
            event: InputEvent::Touch {
                x_fixed, y_fixed, ..
            },
            ..
        }) = queued.last()
        else {
            panic!("latest pending command should be pointer motion");
        };
        assert_eq!((*x_fixed, *y_fixed), (900 << 16, 700 << 16));
    }

    #[test]
    fn stalled_framework_socket_does_not_block_or_starve_controls() {
        use std::time::Duration;

        let (writer, peer) = SeqPacket::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let filler = [0xee; droidloom_input_protocol::RECORD_BYTES];
        let mut prefilled = 0;
        while writer.send_record(&filler, &[]).is_ok() {
            prefilled += 1;
        }
        assert!(prefilled > 0);
        writer.set_nonblocking(false).unwrap();

        let (started_sender, started_receiver) = mpsc::channel();
        let bridge = InputBridge::with_socket_for_test(writer, 4, 2, started_sender);
        bridge
            .send(
                1,
                0,
                7,
                1,
                1,
                10,
                InputEvent::Touch {
                    action: TouchAction::Down,
                    pointer_id: 9,
                    x_fixed: 100 << 16,
                    y_fixed: 200 << 16,
                    pressure: 40_000,
                },
            )
            .unwrap();
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        // The test peer is not reading, so the dedicated writer is stuck in
        // sendmsg. Graph input still enqueues immediately, and lifecycle
        // commands use a separate lane that the writer drains first.
        bridge
            .send(
                2,
                0,
                7,
                1,
                1,
                11,
                InputEvent::Touch {
                    action: TouchAction::Up,
                    pointer_id: 9,
                    x_fixed: 100 << 16,
                    y_fixed: 200 << 16,
                    pressure: 0,
                },
            )
            .unwrap();
        bridge.send_task_bounds(7, 0, 640, 480, 1, 1).unwrap();
        bridge.send_task_focus(7, true).unwrap();

        // Resume the synthetic framework peer. The blocked writer drains the
        // prefix first, then the exact queued input and control records.
        for _ in 0..prefilled {
            peer.receive_record().unwrap();
        }
        let records = (0..4)
            .map(|_| peer.receive_record().unwrap().bytes)
            .collect::<Vec<_>>();
        assert_eq!(records[0][7], TouchAction::Down as u8);
        assert_eq!(
            records[1],
            encode_task_bounds(7, 0, 640, 480, 1, 1).unwrap()
        );
        assert_eq!(records[2], encode_task_focus(7, true).unwrap());
        assert_eq!(records[3][7], TouchAction::Up as u8);
    }

    #[test]
    fn stalled_framework_send_times_out_and_releases_writer() {
        use std::time::Instant;

        let (writer, _peer) = SeqPacket::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let filler = [0xee; droidloom_input_protocol::RECORD_BYTES];
        while writer.send_record(&filler, &[]).is_ok() {}
        writer.set_nonblocking(false).unwrap();
        writer
            .set_send_timeout(Some(std::time::Duration::from_millis(20)))
            .unwrap();

        let started = Instant::now();
        assert!(writer.send_record(&filler, &[]).is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }

    #[test]
    fn mouse_motion_coalesces_only_between_mouse_transitions() {
        let mouse = |action, x: i32| InputEvent::Mouse {
            action,
            x_fixed: x << 16,
            y_fixed: 0,
            button: if matches!(action, MouseAction::ButtonPress) { 0x110 } else { 0 },
            scroll_x_fixed: 0,
            scroll_y_fixed: 0,
        };
        let bridge = InputBridge::with_capacity_for_test(4, 1);
        bridge.send(1, 0, 7, 1, 1, 1, mouse(MouseAction::Motion, 1)).unwrap();
        bridge.send(2, 0, 7, 1, 1, 2, mouse(MouseAction::ButtonPress, 2)).unwrap();
        bridge.send(3, 0, 7, 1, 1, 3, mouse(MouseAction::Motion, 3)).unwrap();
        // The queue is past its coalescing threshold, so this motion replaces
        // the pending one after the press instead of crossing it.
        bridge.send(4, 0, 7, 1, 1, 4, mouse(MouseAction::Motion, 4)).unwrap();
        let mut serials = Vec::new();
        while let Some(BridgeCommand::Input { route_serial, .. }) = bridge.queue.try_pop() {
            serials.push(route_serial);
        }
        assert_eq!(serials, [1, 2, 4]);
    }

    #[test]
    fn mouse_records_carry_scaled_coordinates_and_raw_scroll() {
        let (sender, receiver) = SeqPacket::pair().unwrap();
        let mut state = BridgeState::default();
        state.socket = Some(sender);
        state
            .send(
                Path::new("/unused"),
                1,
                0,
                29,
                2,
                1,
                5,
                InputEvent::Mouse {
                    action: MouseAction::Scroll,
                    x_fixed: 100 << 16,
                    y_fixed: 50 << 16,
                    button: 0,
                    scroll_x_fixed: 0,
                    scroll_y_fixed: -(2 << 16),
                },
            )
            .unwrap();
        let record = receiver.receive_record().unwrap().bytes;
        assert_eq!(record[6], droidloom_input_protocol::KIND_MOUSE);
        assert_eq!(record[7], MouseAction::Scroll as u8);
        assert_eq!(&record[24..28], &(200_i32 << 16).to_le_bytes());
        assert_eq!(&record[28..32], &(100_i32 << 16).to_le_bytes());
        assert_eq!(&record[36..40], &(-(2_i32 << 16)).to_le_bytes());
    }

    #[test]
    fn resumed_tablet_down_sends_proximity_before_contact() {
        let (sender, receiver) = SeqPacket::pair().unwrap();
        let mut state = BridgeState::default();
        state.socket = Some(sender);
        state
            .send(
                Path::new("/unused"),
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
