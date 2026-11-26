//! Direct, nonblocking input writer. Composer only delegates the socket once.
use droidloom_denial_ipc::{IpcError, SeqPacket};
use droidloom_denial_protocol::{InputEvent, TouchAction};
use droidloom_input_protocol::{
    MAX_POINTER_ID, PROTOCOL_MAJOR, RECORD_BYTES, encode_display_power, encode_key, encode_scroll,
    encode_touch, scale_fixed,
};
use std::{
    collections::{BTreeMap, VecDeque},
    os::fd::{AsFd, AsRawFd, OwnedFd, RawFd},
};

const QUEUE_LIMIT: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Route {
    pub task: u64,
    pub display: u32,
    pub incarnation: u64,
}

#[derive(Default)]
pub(super) struct DirectInput {
    socket: Option<SeqPacket>,
    queue: VecDeque<(Option<Route>, [u8; RECORD_BYTES])>,
    pointers: BTreeMap<(u32, u32), (u32, Route)>,
    pens: BTreeMap<u32, Route>,
    generation: u64,
    requested: bool,
    power: Option<bool>,
    wire: u16,
    blocked: bool,
}

impl DirectInput {
    pub fn set_blocked(&mut self, blocked: bool) {
        self.blocked = blocked;
    }

    pub fn next_request(&mut self) -> Option<u64> {
        if self.socket.is_some() || self.requested {
            return None;
        }
        self.generation = self
            .generation
            .checked_add(1)
            .expect("input generation exhausted");
        self.requested = true;
        Some(self.generation)
    }

    pub fn attach(&mut self, generation: u64, wire: u16, fd: OwnedFd) -> Result<bool, String> {
        if generation != self.generation || !self.requested {
            return Ok(false);
        }
        if wire != PROTOCOL_MAJOR {
            return Err(format!("native input wire ABI {wire} != {PROTOCOL_MAJOR}"));
        }
        let socket = SeqPacket::from_owned_fd(fd).map_err(|e| e.to_string())?;
        if socket.peer_credentials().map_err(|e| e.to_string())?.pid == 0 {
            return Err("native input descriptor is not connected".into());
        }
        socket.set_nonblocking(true).map_err(|e| e.to_string())?;
        self.disconnect();
        self.socket = Some(socket);
        self.wire = wire;
        if let Some(power) = self.power {
            self.enqueue(None, encode_display_power(power))?;
        }
        Ok(true)
    }

    pub fn disconnect(&mut self) {
        self.socket = None; // EOF makes InputFlinger cancel held input.
        self.queue.clear();
        self.pointers.clear();
        self.pens.clear();
        self.requested = false;
    }

    pub fn supports_stylus(&self) -> bool {
        self.socket.is_some() && self.wire == PROTOCOL_MAJOR
    }

    pub fn send_stylus(
        &mut self,
        mut route: Route,
        scale: (u32, u32),
        timestamp: u64,
        mut pen: droidloom_input_protocol::Stylus,
    ) -> Result<(), String> {
        if !self.supports_stylus() {
            return Err(format!("native stylus requires input wire ABI {PROTOCOL_MAJOR}"));
        }
        if pen.action == 0 {
            if let Some(old) = self.pens.remove(&pen.tool) {
                let cancel = droidloom_input_protocol::encode_stylus(
                    old.display,
                    old.task,
                    timestamp,
                    old.incarnation,
                    droidloom_input_protocol::Stylus { action: 5, ..pen },
                )
                .map_err(|e| e.to_string())?;
                self.enqueue(Some(old), cancel)?;
            }
            self.pens.insert(pen.tool, route);
        } else if let Some(original) = self.pens.get(&pen.tool) {
            route = *original;
        } else {
            self.pens.insert(pen.tool, route);
            let enter_pen = droidloom_input_protocol::Stylus { action: 0, ..pen };
            let enter_record = droidloom_input_protocol::encode_stylus(
                route.display,
                route.task,
                timestamp,
                route.incarnation,
                enter_pen,
            )
            .map_err(|e| e.to_string())?;
            self.enqueue(Some(route), enter_record)?;
        }
        pen.x_fixed = scale_fixed(pen.x_fixed, scale.0, scale.1).map_err(|e| e.to_string())?;
        pen.y_fixed = scale_fixed(pen.y_fixed, scale.0, scale.1).map_err(|e| e.to_string())?;
        let record = droidloom_input_protocol::encode_stylus(
            route.display,
            route.task,
            timestamp,
            route.incarnation,
            pen,
        )
        .map_err(|e| e.to_string())?;
        if matches!(pen.action, 2 | 5) {
            self.pens.remove(&pen.tool);
        }
        self.enqueue(Some(route), record)
    }

    pub fn set_power(&mut self, power: bool) -> Result<(), String> {
        if self.power == Some(power) {
            return Ok(());
        }
        self.power = Some(power);
        if self.socket.is_some() {
            self.enqueue(None, encode_display_power(power))?;
        }
        if !power {
            self.pointers.clear(); // The receiver resets its shared gesture on power-off.
        }
        Ok(())
    }

    pub fn cancel_task_pointer(
        &mut self,
        task: u64,
        pointer_id: u32,
        timestamp: u64,
    ) -> Result<bool, String> {
        if self.socket.is_none() {
            return Err("native input is reconnecting".into());
        }
        let Some((android_id, route)) = self
            .pointers
            .iter()
            .find(|((_, host_id), (_, owner))| *host_id == pointer_id && owner.task == task)
            .map(|(_, contact)| *contact)
        else {
            return Ok(false);
        };
        let record = encode_touch(
            route.display,
            route.task,
            timestamp,
            TouchAction::Cancel as u8,
            android_id,
            0,
            0,
            0,
            route.incarnation,
        )
        .map_err(|error| error.to_string())?;
        self.enqueue(Some(route), record)?;
        self.pointers.retain(|(_, _), (_, owner)| *owner != route);
        Ok(true)
    }

    pub fn cancel_all(&mut self, timestamp: u64) -> Result<(), String> {
        while let Some((_, (_, route))) = self.pointers.first_key_value() {
            let display = route.display;
            self.cancel_display(display, timestamp)?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn retire_route(&mut self, route: Route, timestamp: u64) -> Result<(), String> {
        if self.pointers.values().any(|(_, owner)| *owner == route) {
            self.cancel_display(route.display, timestamp)?;
        }
        Ok(())
    }

    fn cancel_display(&mut self, display: u32, timestamp: u64) -> Result<(), String> {
        // Android's injected device/verifier stream is shared across task routes.
        // Cancel each route, including the accepted owner and any ignored
        // cross-task lifetimes. Mouse-as-touch participates in the same stream.
        let mut routes = Vec::new();
        for ((owned_display, _), (id, route)) in &self.pointers {
            if *owned_display == display && !routes.iter().any(|(_, owner)| owner == route) {
                routes.push((*id, *route));
            }
        }
        for (id, route) in routes {
            let record = encode_touch(
                display,
                route.task,
                timestamp,
                TouchAction::Cancel as u8,
                id,
                0,
                0,
                0,
                route.incarnation,
            )
            .map_err(|e| e.to_string())?;
            self.enqueue(Some(route), record)?;
        }
        self.pointers
            .retain(|(owned_display, _), _| *owned_display != display);
        Ok(())
    }

    pub fn send(
        &mut self,
        mut route: Route,
        scale: (u32, u32),
        serial: u64,
        timestamp: u64,
        event: InputEvent,
    ) -> Result<(), String> {
        if self.socket.is_none() {
            return Err("native input is reconnecting".into());
        }
        let record = match event {
            InputEvent::Touch {
                action,
                pointer_id,
                x_fixed,
                y_fixed,
                pressure,
            } => {
                if action == TouchAction::Cancel {
                    return self.cancel_display(route.display, timestamp);
                }
                // Validate before changing ownership; invalid input must not leave
                // a local pointer that was never delivered to the receiver.
                let (x, y) = match (
                    scale_fixed(x_fixed, scale.0, scale.1),
                    scale_fixed(y_fixed, scale.0, scale.1),
                ) {
                    (Ok(x), Ok(y)) => (x, y),
                    _ => return self.fail("invalid touch scale or coordinates"),
                };
                let key = (route.display, pointer_id);
                let android_id = match action {
                    TouchAction::Down => {
                        // Validate the route before changing ownership; an
                        // invalid identity must not leave a local contact that
                        // was never delivered to the receiver.
                        if let Err(error) = encode_touch(
                            route.display,
                            route.task,
                            timestamp,
                            action as u8,
                            0,
                            x,
                            y,
                            pressure,
                            route.incarnation,
                        ) {
                            return self.fail(&format!("invalid native input: {error}"));
                        }
                        if let Some((id, original)) = self.pointers.get(&key).copied() {
                            // A lost Wayland up must cancel only the affected
                            // Android gesture, not the shared keyboard channel.
                            let cancel = encode_touch(
                                original.display,
                                original.task,
                                timestamp,
                                TouchAction::Cancel as u8,
                                id,
                                0,
                                0,
                                0,
                                original.incarnation,
                            )
                            .map_err(|e| e.to_string())?;
                            self.enqueue(Some(original), cancel)?;
                            self.pointers.retain(|_, (_, owned)| *owned != original);
                        }
                        let Some(id) = (0..=MAX_POINTER_ID).find(|candidate| {
                            !self.pointers.iter().any(|((display, _), (id, _))| {
                                *display == route.display && id == candidate
                            })
                        }) else {
                            return self.fail("touch identities exhausted");
                        };
                        self.pointers.insert(key, (id, route));
                        id
                    }
                    TouchAction::Motion | TouchAction::Up | TouchAction::Cancel => {
                        let Some((id, original)) = self.pointers.get(&key).copied() else {
                            return Ok(());
                        };
                        route = original;
                        if action == TouchAction::Up {
                            self.pointers.remove(&key);
                        }
                        id
                    }
                };
                encode_touch(
                    route.display,
                    route.task,
                    timestamp,
                    action as u8,
                    android_id,
                    x,
                    y,
                    pressure,
                    route.incarnation,
                )
            }
            InputEvent::Key {
                action,
                keycode,
                repeat,
            } => encode_key(
                route.display,
                route.task,
                timestamp,
                action as u8,
                keycode,
                repeat,
                serial,
                route.incarnation,
            ),
            InputEvent::Scroll {
                x_fixed,
                y_fixed,
                vertical_fixed,
                horizontal_fixed,
            } => {
                let (x, y) = match (
                    scale_fixed(x_fixed, scale.0, scale.1),
                    scale_fixed(y_fixed, scale.0, scale.1),
                ) {
                    (Ok(x), Ok(y)) => (x, y),
                    _ => return self.fail("invalid scroll scale or coordinates"),
                };
                encode_scroll(
                    route.display,
                    route.task,
                    timestamp,
                    x,
                    y,
                    vertical_fixed,
                    horizontal_fixed,
                    route.incarnation,
                )
            }
            InputEvent::Navigation { .. } => {
                return Err("navigation injection is not implemented".into());
            }
        }
        .map_err(|e| e.to_string())?;
        self.enqueue(Some(route), record)
    }

    fn enqueue(
        &mut self,
        route: Option<Route>,
        mut record: [u8; RECORD_BYTES],
    ) -> Result<(), String> {
        record[4..6].copy_from_slice(&self.wire.to_le_bytes());
        self.flush()?;
        if self.queue.len() == QUEUE_LIMIT {
            return self.fail("native input queue exhausted; cancelling stream");
        }
        self.queue.push_back((route, record));
        self.flush()
    }

    pub fn flush(&mut self) -> Result<(), String> {
        if self.blocked {
            return Ok(());
        }
        while let (Some(socket), Some((_, record))) = (self.socket.as_ref(), self.queue.front()) {
            match socket.send_record(record, &[]) {
                Ok(()) => {
                    self.queue.pop_front();
                }
                Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return self.fail(&format!("native input disconnected: {error}")),
            }
        }
        Ok(())
    }

    fn fail<T>(&mut self, message: &str) -> Result<T, String> {
        self.disconnect();
        Err(message.into())
    }

    pub fn fd(&self) -> Option<(RawFd, i16)> {
        self.socket.as_ref().map(|s| {
            (
                s.as_fd().as_raw_fd(),
                libc::POLLIN
                    | if self.queue.is_empty() || self.blocked {
                        0
                    } else {
                        libc::POLLOUT
                    },
            )
        })
    }

    pub fn notify_ready(&mut self, descriptors: &[libc::pollfd]) {
        if let Some((fd, _)) = self.fd() {
            if descriptors.iter().any(|p| {
                p.fd == fd
                    && p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
                        != 0
            }) {
                self.disconnect();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use droidloom_denial_ipc::ProtocolSocket;
    use droidloom_denial_protocol::{AndroidMessage, KeyAction};
    use droidloom_input_protocol::{Payload, Record, decode};

    #[test]
    fn clipboard_barrier_orders_keys_and_pen_without_writable_spin() {
        let (mut input, receiver) = connected();
        receiver.set_nonblocking(true).unwrap();
        input.set_blocked(true);
        input.send(route(), (1, 1), 1, 100, key()).unwrap();
        input
            .send_stylus(
                route(),
                (1, 1),
                102,
                droidloom_input_protocol::Stylus {
                    tool: 1,
                    action: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        input.flush().unwrap();
        assert!(
            receiver.receive_record().is_err(),
            "input must wait for clipboard"
        );
        assert_eq!(input.fd().unwrap().1 & libc::POLLOUT, 0);
        input.set_blocked(false);
        input.flush().unwrap();
        assert!(matches!(sample(&receiver).payload, Payload::Key { .. }));
        assert!(matches!(sample(&receiver).payload, Payload::Stylus { .. }));
        assert!(receiver.receive_record().is_err());
    }

    #[test]
    fn clipboard_barrier_reconnect_never_replays_a_held_modifier() {
        let (mut input, old_receiver) = connected();
        old_receiver.set_nonblocking(true).unwrap();
        input.set_blocked(true);
        input.send(route(), (1, 1), 1, 100, key()).unwrap();
        assert!(old_receiver.receive_record().is_err());
        input.disconnect();
        let generation = input.next_request().unwrap();
        let (writer, receiver) = SeqPacket::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        input
            .attach(
                generation,
                PROTOCOL_MAJOR,
                writer.as_fd().try_clone_to_owned().unwrap(),
            )
            .unwrap();
        input.set_blocked(false);
        input.flush().unwrap();
        assert!(
            receiver.receive_record().is_err(),
            "new receiver must not inherit presses"
        );
        input.send(route(), (1, 1), 2, 101, key()).unwrap();
        assert!(matches!(
            sample(&receiver).payload,
            Payload::Key { serial: 2, .. }
        ));
    }

    #[test]
    fn native_pen_socket_scales_axes_and_keeps_the_original_route() {
        let (mut input, receiver) = connected();
        let pen = droidloom_input_protocol::Stylus {
            tool: 99,
            action: 0,
            x_fixed: 2 << 16,
            y_fixed: 3 << 16,
            pressure: 32768,
            tilt_x: 30,
            tilt_y: -45,
            eraser: true,
            buttons: 1,
        };
        input.send_stylus(route(), (3, 2), 100, pen).unwrap();
        let entered = sample(&receiver);
        assert_eq!(entered.code, 99);
        assert_eq!(entered.action, 0);
        let Payload::Stylus {
            x,
            y,
            tilt_x,
            tilt_y,
            eraser,
            buttons,
            ..
        } = entered.payload
        else {
            panic!("native stylus")
        };
        assert_eq!(
            (x, y, tilt_x, tilt_y, eraser, buttons),
            (3.0, 4.5, 30.0, -45.0, true, 1)
        );
        // Focus changes must not redirect the tool's release to another task.
        input
            .send_stylus(
                Route {
                    task: 43,
                    ..route()
                },
                (3, 2),
                101,
                droidloom_input_protocol::Stylus { action: 4, ..pen },
            )
            .unwrap();
        assert_eq!(sample(&receiver).task, 42);
        input.send(route(), (1, 1), 3, 103, key()).unwrap();
        assert!(matches!(sample(&receiver).payload, Payload::Key { .. }));
    }

    #[test]
    fn lost_up_resynchronizes_one_gesture_without_disconnecting_keyboard() {
        let (mut input, receiver) = connected();
        input
            .send(route(), (1, 1), 1, 100, touch(7, TouchAction::Down))
            .unwrap();
        sample(&receiver);
        let other = Route {
            task: 43,
            ..route()
        };
        input
            .send(other, (1, 1), 2, 101, touch(8, TouchAction::Down))
            .unwrap();
        sample(&receiver);
        input
            .send(route(), (1, 1), 3, 102, touch(7, TouchAction::Down))
            .unwrap();
        let cancelled = sample(&receiver);
        assert_eq!(
            (cancelled.task, cancelled.action),
            (42, TouchAction::Cancel as u8)
        );
        assert_eq!(sample(&receiver).action, TouchAction::Down as u8);
        assert!(input.fd().is_some());
        input
            .send(other, (1, 1), 4, 103, touch(8, TouchAction::Up))
            .unwrap();
        assert_eq!(sample(&receiver).task, 43);
        input.send(route(), (1, 1), 5, 104, key()).unwrap();
        assert!(matches!(
            sample(&receiver).payload,
            Payload::Key { serial: 5, .. }
        ));
    }

    #[test]
    fn mismatched_wire_is_rejected_before_any_record() {
        let (writer, _receiver) = SeqPacket::pair().unwrap();
        let mut input = DirectInput::default();
        let generation = input.next_request().unwrap();
        assert!(input
            .attach(
                generation,
                PROTOCOL_MAJOR - 1,
                writer.as_fd().try_clone_to_owned().unwrap(),
            )
            .is_err());
        assert!(input.fd().is_none());
        assert!(!input.supports_stylus());
    }


    fn route() -> Route {
        Route {
            task: 42,
            display: 0,
            incarnation: 7,
        }
    }
    fn key() -> InputEvent {
        InputEvent::Key {
            action: KeyAction::Down,
            keycode: 30,
            repeat: 0,
        }
    }
    fn touch(id: u32, action: TouchAction) -> InputEvent {
        InputEvent::Touch {
            action,
            pointer_id: id,
            x_fixed: 2 << 16,
            y_fixed: -(3 << 16),
            pressure: u16::MAX,
        }
    }
    fn connected() -> (DirectInput, SeqPacket) {
        let (writer, receiver) = SeqPacket::pair().unwrap();
        receiver
            .set_io_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let mut input = DirectInput::default();
        let generation = input.next_request().unwrap();
        input
            .attach(
                generation,
                PROTOCOL_MAJOR,
                writer.as_fd().try_clone_to_owned().unwrap(),
            )
            .unwrap();
        (input, receiver)
    }
    fn sample(receiver: &SeqPacket) -> droidloom_input_protocol::Sample {
        let record = receiver.receive_record().unwrap();
        let Record::Input(sample) = decode(&record.bytes).unwrap() else {
            panic!("expected input");
        };
        sample
    }
    fn small_send_buffer(input: &DirectInput) {
        let fd = input.fd().unwrap().0;
        let size: i32 = 4096;
        // SAFETY: live socket, exact initialized integer buffer and size.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&raw const size).cast(),
                    libc::socklen_t::try_from(std::mem::size_of_val(&size)).unwrap(),
                )
            },
            0
        );
    }

    #[test]
    fn handoff_transfers_the_writer_and_does_not_retain_a_composer_duplicate() {
        let (composer, host) = SeqPacket::pair().unwrap();
        let composer = ProtocolSocket::new(composer);
        let host = ProtocolSocket::new(host);
        let (writer, receiver) = SeqPacket::pair().unwrap();
        let mut input = DirectInput::default();
        let generation = input.next_request().unwrap();
        composer
            .send_android(
                &AndroidMessage::BindInputChannel {
                    generation,
                    wire: PROTOCOL_MAJOR,
                },
                &[writer.as_fd()],
            )
            .unwrap();
        drop(writer);
        let handoff = host.receive_android().unwrap();
        input
            .attach(
                generation,
                PROTOCOL_MAJOR,
                handoff.descriptors.into_iter().next().unwrap().fd,
            )
            .unwrap();
        input.send(route(), (1, 1), 55, 100, key()).unwrap();
        assert_eq!(
            sample(&receiver).payload,
            Payload::Key {
                repeat: 0,
                serial: 55
            }
        );
        input.disconnect();
        assert!(receiver.receive_record().is_err()); // Native receiver can now cancel held input.
    }

    #[test]
    fn scroll_is_scaled_to_buffer_pixels_and_keeps_its_notches() {
        let (mut input, receiver) = connected();
        let scroll = InputEvent::Scroll {
            x_fixed: 10 << 16,
            y_fixed: 4 << 16,
            vertical_fixed: -(1 << 15),
            horizontal_fixed: 1 << 16,
        };
        input.send(route(), (3, 2), 1, 100, scroll).unwrap();
        let sample = sample(&receiver);
        assert_eq!((sample.task, sample.incarnation), (42, 7));
        assert_eq!(
            sample.payload,
            Payload::Scroll {
                x: 15.0,
                y: 6.0,
                vertical: -0.5,
                horizontal: 1.0
            }
        );
    }

    #[test]
    fn fractional_scale_pointer_ids_and_original_incarnations_survive_focus_changes() {
        let (mut input, receiver) = connected();
        input
            .send(route(), (3, 2), 1, 100, touch(u32::MAX, TouchAction::Down))
            .unwrap();
        let down = sample(&receiver);
        assert_eq!((down.task, down.incarnation, down.code), (42, 7, 0));
        assert_eq!(
            down.payload,
            Payload::Touch {
                x: 3.0,
                y: -4.5,
                pressure: 1.0
            }
        );
        input
            .send(route(), (1, 1), 2, 101, touch(99, TouchAction::Down))
            .unwrap();
        assert_eq!(sample(&receiver).code, 1);
        input
            .send(
                Route {
                    task: 43,
                    incarnation: 8,
                    ..route()
                },
                (1, 1),
                3,
                102,
                touch(u32::MAX, TouchAction::Up),
            )
            .unwrap();
        let up = sample(&receiver);
        assert_eq!((up.task, up.incarnation, up.code), (42, 7, 0));
    }

    #[test]
    fn backpressure_keeps_fifo_order_and_wakes_for_writability_only_while_queued() {
        let (mut input, receiver) = connected();
        small_send_buffer(&input);
        assert_eq!(input.fd().unwrap().1 & libc::POLLOUT, 0);
        for serial in 1..=80 {
            input.send(route(), (1, 1), serial, serial, key()).unwrap();
        }
        assert!(!input.queue.is_empty());
        assert_ne!(input.fd().unwrap().1 & libc::POLLOUT, 0);
        for serial in 1..=80 {
            assert_eq!(
                sample(&receiver).payload,
                Payload::Key { repeat: 0, serial }
            );
            input.flush().unwrap();
        }
        assert!(input.queue.is_empty());
        assert_eq!(input.fd().unwrap().1 & libc::POLLOUT, 0);
    }

    #[test]
    fn overflow_cancels_the_connection_and_stale_handoffs_cannot_rebind_it() {
        let (mut input, _receiver) = connected();
        small_send_buffer(&input);
        let previous = input.generation;
        let mut cancelled = false;
        for serial in 1..=1000 {
            if input.send(route(), (1, 1), serial, serial, key()).is_err() {
                cancelled = true;
                break;
            }
        }
        assert!(cancelled && input.fd().is_none() && input.queue.is_empty());
        let generation = input.next_request().unwrap();
        assert!(generation > previous);
        let (writer, _) = SeqPacket::pair().unwrap();
        assert!(
            !input
                .attach(
                    previous,
                    PROTOCOL_MAJOR,
                    writer.as_fd().try_clone_to_owned().unwrap()
                )
                .unwrap()
        );
        assert!(
            input
                .attach(
                    generation,
                    PROTOCOL_MAJOR + 1,
                    writer.as_fd().try_clone_to_owned().unwrap()
                )
                .is_err()
        );
    }

    #[test]
    fn disconnect_replays_power_on_the_new_connection_and_has_no_idle_write_wakeup() {
        let (mut input, receiver) = connected();
        input.set_power(false).unwrap();
        assert_eq!(
            decode(&receiver.receive_record().unwrap().bytes).unwrap(),
            Record::DisplayPower(false)
        );
        let fd = input.fd().unwrap().0;
        drop(receiver);
        input.notify_ready(&[libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: libc::POLLHUP,
        }]);
        let generation = input.next_request().unwrap();
        let (writer, receiver) = SeqPacket::pair().unwrap();
        input
            .attach(
                generation,
                PROTOCOL_MAJOR,
                writer.as_fd().try_clone_to_owned().unwrap(),
            )
            .unwrap();
        assert_eq!(
            decode(&receiver.receive_record().unwrap().bytes).unwrap(),
            Record::DisplayPower(false)
        );
        assert_eq!(input.fd().unwrap().1, libc::POLLIN);
    }

    #[test]
    fn route_retirement_cancels_shared_display_routes_and_pointer_but_not_other_displays() {
        let (mut input, receiver) = connected();
        let other = Route {
            task: 43,
            incarnation: 8,
            ..route()
        };
        let separate = Route {
            display: 1,
            ..route()
        };
        // Owner's ID is deliberately not the first key in the contact map.
        for (owner, id) in [(route(), 9), (route(), u32::MAX), (other, 1), (separate, 3)] {
            input
                .send(owner, (1, 1), 0, 100, touch(id, TouchAction::Down))
                .unwrap();
            sample(&receiver);
        }
        input.retire_route(other, 101).unwrap();
        let first = sample(&receiver);
        let second = sample(&receiver);
        assert_eq!(
            (first.action, second.action),
            (TouchAction::Cancel as u8, TouchAction::Cancel as u8)
        );
        assert_eq!((first.task, second.task), (43, 42));
        assert_eq!((first.incarnation, second.incarnation), (8, 7));
        assert_eq!(input.pointers.len(), 1);
        assert!(input.pointers.contains_key(&(1, 3)));
        for (owner, id) in [(route(), 9), (route(), u32::MAX), (other, 1)] {
            input
                .send(owner, (1, 1), 0, 102, touch(id, TouchAction::Up))
                .unwrap();
        }
        input
            .send(route(), (1, 1), 0, 103, touch(9, TouchAction::Down))
            .unwrap();
        let next = sample(&receiver);
        assert_eq!((next.action, next.code), (TouchAction::Down as u8, 0));
        input
            .send(route(), (1, 1), 0, 104, touch(9, TouchAction::Up))
            .unwrap();
        assert_eq!(sample(&receiver).action, TouchAction::Up as u8);
        input.retire_route(other, 105).unwrap(); // Already retired, no duplicate CANCEL.
        receiver.set_nonblocking(true).unwrap();
        assert!(
            matches!(receiver.receive_record(), Err(IpcError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn cancel_after_two_completed_down_records_retires_all_contacts_and_mouse() {
        let (mut input, receiver) = connected();
        for id in [0, 1, u32::MAX] {
            input
                .send(route(), (1, 1), 0, 100, touch(id, TouchAction::Down))
                .unwrap();
            sample(&receiver); // Each event is fully flushed, like down; frame().
        }
        input
            .send(route(), (0, 0), 0, 101, touch(99, TouchAction::Cancel))
            .unwrap();
        assert_eq!(sample(&receiver).action, TouchAction::Cancel as u8);
        assert!(input.pointers.is_empty());
        input
            .send(route(), (1, 1), 0, 102, touch(0, TouchAction::Down))
            .unwrap();
        assert_eq!(sample(&receiver).code, 0);
        input
            .send(route(), (1, 1), 0, 103, touch(0, TouchAction::Up))
            .unwrap();
        assert_eq!(sample(&receiver).action, TouchAction::Up as u8);
    }

    #[test]
    fn capability_or_seat_retirement_orders_cancel_after_queued_downs() {
        let (mut input, receiver) = connected();
        small_send_buffer(&input);
        for serial in 1..=80 {
            input.send(route(), (1, 1), serial, serial, key()).unwrap();
        }
        for id in [0, 1, u32::MAX] {
            input
                .send(route(), (1, 1), 0, 100, touch(id, TouchAction::Down))
                .unwrap();
        }
        input.cancel_all(101).unwrap();
        assert!(input.pointers.is_empty());
        input
            .send(route(), (1, 1), 0, 102, touch(0, TouchAction::Down))
            .unwrap();
        for _ in 0..80 {
            sample(&receiver);
            input.flush().unwrap();
        }
        for code in 0..3 {
            let down = sample(&receiver);
            assert_eq!((down.action, down.code), (TouchAction::Down as u8, code));
            input.flush().unwrap();
        }
        assert_eq!(sample(&receiver).action, TouchAction::Cancel as u8);
        let down = sample(&receiver);
        assert_eq!((down.action, down.code), (TouchAction::Down as u8, 0));
    }

    /// Unrepresentable downs tear the channel down instead of leaving a
    /// phantom contact behind. A duplicated host identity is not one of them:
    /// `lost_up_resynchronizes_one_gesture_without_disconnecting_keyboard`
    /// covers that recoverable case.
    #[test]
    fn invalid_downs_disconnect_without_phantom_contacts_then_recover() {
        for failure in 0..4 {
            let (mut input, receiver) = connected();
            input
                .send(route(), (1, 1), 0, 100, touch(0, TouchAction::Down))
                .unwrap();
            sample(&receiver);
            let (owner, scale, id) = match failure {
                // Android task ID outside the positive framework range.
                0 => (
                    Route {
                        task: 0,
                        ..route()
                    },
                    (1, 1),
                    1,
                ),
                1 => (route(), (0, 1), 1),
                2 => (route(), (u32::MAX, 1), 1),
                _ => (
                    Route {
                        incarnation: 0,
                        ..route()
                    },
                    (1, 1),
                    1,
                ),
            };
            assert!(
                input
                    .send(owner, scale, 0, 101, touch(id, TouchAction::Down))
                    .is_err()
            );
            assert!(input.fd().is_none() && input.pointers.is_empty() && input.queue.is_empty());
            assert!(receiver.receive_record().is_err()); // EOF retires receiver's stream.
            let generation = input.next_request().unwrap();
            let (writer, next_receiver) = SeqPacket::pair().unwrap();
            input
                .attach(
                    generation,
                    PROTOCOL_MAJOR,
                    writer.as_fd().try_clone_to_owned().unwrap(),
                )
                .unwrap();
            input
                .send(route(), (1, 1), 0, 102, touch(0, TouchAction::Down))
                .unwrap();
            assert_eq!(sample(&next_receiver).code, 0);
            input
                .send(route(), (1, 1), 0, 103, touch(0, TouchAction::Up))
                .unwrap();
            assert_eq!(sample(&next_receiver).action, TouchAction::Up as u8);
        }
    }

    #[test]
    fn power_off_retires_contacts_and_pointer_then_accepts_a_fresh_down() {
        let (mut input, receiver) = connected();
        for id in [0, u32::MAX] {
            input
                .send(route(), (1, 1), 0, 100, touch(id, TouchAction::Down))
                .unwrap();
            sample(&receiver);
        }
        input.set_power(false).unwrap();
        assert_eq!(
            decode(&receiver.receive_record().unwrap().bytes).unwrap(),
            Record::DisplayPower(false)
        );
        assert!(input.pointers.is_empty());
        input.set_power(true).unwrap();
        assert_eq!(
            decode(&receiver.receive_record().unwrap().bytes).unwrap(),
            Record::DisplayPower(true)
        );
        input
            .send(route(), (1, 1), 0, 102, touch(0, TouchAction::Down))
            .unwrap();
        assert_eq!(sample(&receiver).code, 0);
    }
}
