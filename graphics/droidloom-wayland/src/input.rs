//! Direct, nonblocking input writer. Composer only delegates the socket once.
use droidloom_denial_ipc::{IpcError, SeqPacket};
use droidloom_denial_protocol::{InputEvent, TouchAction};
use droidloom_input_protocol::{
    MAX_POINTER_ID, PROTOCOL_MAJOR, RECORD_BYTES, encode_display_power, encode_key, encode_touch,
    scale_fixed,
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
    queue: VecDeque<[u8; RECORD_BYTES]>,
    pointers: BTreeMap<(u32, u32), (u32, Route)>,
    generation: u64,
    requested: bool,
    power: Option<bool>,
}

impl DirectInput {
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
        if let Some(power) = self.power {
            self.enqueue(encode_display_power(power))?;
        }
        Ok(true)
    }

    pub fn disconnect(&mut self) {
        self.socket = None; // EOF makes InputFlinger cancel held input.
        self.queue.clear();
        self.pointers.clear();
        self.requested = false;
    }

    pub fn set_power(&mut self, power: bool) -> Result<(), String> {
        if self.power == Some(power) {
            return Ok(());
        }
        self.power = Some(power);
        if self.socket.is_some() {
            self.enqueue(encode_display_power(power))?;
        }
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
                let key = (route.display, pointer_id);
                let android_id = match action {
                    TouchAction::Down => {
                        if self.pointers.contains_key(&key) {
                            return self.fail("duplicate touch down");
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
                        if action == TouchAction::Cancel {
                            self.pointers.retain(|(_, _), (_, owned)| *owned != route);
                        }
                        id
                    }
                };
                let x = scale_fixed(x_fixed, scale.0, scale.1).map_err(|e| e.to_string())?;
                let y = scale_fixed(y_fixed, scale.0, scale.1).map_err(|e| e.to_string())?;
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
            InputEvent::Navigation { .. } => {
                return Err("navigation injection is not implemented".into());
            }
        }
        .map_err(|e| e.to_string())?;
        self.enqueue(record)
    }

    fn enqueue(&mut self, record: [u8; RECORD_BYTES]) -> Result<(), String> {
        self.flush()?;
        if self.queue.len() == QUEUE_LIMIT {
            return self.fail("native input queue exhausted; cancelling stream");
        }
        self.queue.push_back(record);
        self.flush()
    }

    pub fn flush(&mut self) -> Result<(), String> {
        while let (Some(socket), Some(record)) = (self.socket.as_ref(), self.queue.front()) {
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
                    | if self.queue.is_empty() {
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
}
