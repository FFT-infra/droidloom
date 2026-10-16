//! Exercise the same event/lifecycle helpers as App using real protocol objects
//! on a private socket. No desktop, GPU or Android runtime is needed.
use super::*;
use std::io::Read;
use std::os::unix::net::UnixStream;
use wayland_client::protocol::wl_registry;

struct Client;
wayland_client::delegate_noop!(Client: ignore wl_registry::WlRegistry);
wayland_client::delegate_noop!(Client: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(Client: ignore ZwlrOutputPowerManagerV1);
wayland_client::delegate_noop!(Client: ignore ZwlrOutputPowerV1);

struct Fixture {
    conn: Connection,
    peer: UnixStream,
    _queue: EventQueue<Client>,
    states: Vec<OutputPowerState>,
    controls: Vec<ZwlrOutputPowerV1>,
}

impl Fixture {
    fn new() -> Self {
        let (client, peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let queue = conn.new_event_queue::<Client>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let manager = registry.bind::<ZwlrOutputPowerManagerV1, _, _>(1, 1, &qh, ());
        let mut states = Vec::new();
        let mut controls = Vec::new();
        for global in [2, 3] {
            let output = registry.bind::<wl_output::WlOutput, _, _>(global, 4, &qh, ());
            let control = manager.get_output_power(&output, &qh, ());
            controls.push(control.clone());
            states.push(OutputPowerState {
                output,
                control: Some(control),
                powered: Some(true),
            });
        }
        let mut result = Self {
            conn,
            peer,
            _queue: queue,
            states,
            controls,
        };
        result.requests(); // Discard object creation.
        result
    }

    fn event(&mut self, index: usize, event: zwlr_output_power_v1::Event) -> bool {
        update_output_power(&mut self.states, &self.controls[index], event)
    }

    fn power(&self) -> Option<bool> {
        aggregate_display_power(self.states.iter().map(|state| state.powered))
    }

    fn requests(&mut self) -> Vec<(u32, u16)> {
        self.conn.flush().unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match self.peer.read(&mut buffer) {
                Ok(0) => panic!("private Wayland connection closed"),
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        let mut result = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let object = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
            let header = u32::from_ne_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            let size = (header >> 16) as usize;
            assert!(size >= 8 && offset + size <= bytes.len());
            result.push((object, (header & 0xffff) as u16));
            offset += size;
        }
        result
    }
}

fn mode(powered: bool) -> zwlr_output_power_v1::Event {
    zwlr_output_power_v1::Event::Mode {
        mode: WEnum::Value(if powered {
            zwlr_output_power_v1::Mode::On
        } else {
            zwlr_output_power_v1::Mode::Off
        }),
    }
}

#[test]
fn monitor_failure_keeps_internal_display_running_and_destroys_control_once() {
    let mut f = Fixture::new();
    let monitor = f.states[1].output.clone();
    let id = f.controls[1].id().protocol_id();
    assert!(f.event(1, zwlr_output_power_v1::Event::Failed));
    assert_eq!(f.power(), Some(true));
    assert!(f.states[1].control.is_none());
    assert!(!f.event(1, zwlr_output_power_v1::Event::Failed));
    assert!(!f.event(1, mode(false))); // Queued late event cannot change state.
    remove_output_power(&mut f.states, &monitor);
    assert_eq!(f.power(), Some(true));
    let requests = f.requests();
    let destroy = ZwlrOutputPowerV1::interface()
        .requests
        .iter()
        .position(|request| request.name == "destroy")
        .unwrap() as u16;
    assert_eq!(requests, [(id, destroy)]);
    assert!(f.event(0, mode(false))); // Ordinary remaining-panel DPMS still works.
    assert_eq!(f.power(), Some(false));
    assert!(f.event(0, mode(true)));
    assert_eq!(f.power(), Some(true));
}

#[test]
fn output_removal_before_failed_event_ignores_the_late_failure() {
    let mut f = Fixture::new();
    let monitor = f.states[1].output.clone();
    remove_output_power(&mut f.states, &monitor);
    assert!(!f.event(1, zwlr_output_power_v1::Event::Failed));
    assert_eq!(f.requests().len(), 1);
    assert_eq!(f.power(), Some(true));
    let panel = f.states[0].output.clone();
    remove_output_power(&mut f.states, &panel);
    assert_eq!(f.power(), None); // No invented off event with no known outputs.
    assert!(!f.event(0, mode(false)));
}

#[test]
fn unsupported_power_control_or_unknown_mode_does_not_mean_screen_off() {
    let mut f = Fixture::new();
    assert!(f.event(0, mode(false)));
    assert!(f.event(1, zwlr_output_power_v1::Event::Failed));
    assert_eq!(f.power(), None); // Output may still exist without power support.
    assert!(f.event(0, mode(true)));
    assert_eq!(f.power(), Some(true));
    assert!(f.event(
        0,
        zwlr_output_power_v1::Event::Mode {
            mode: WEnum::Unknown(9)
        }
    ));
    assert_eq!(f.power(), None);
    assert!(f.states[0].control.is_some()); // A later understood mode can recover.
    assert!(f.event(0, mode(true)));
    assert_eq!(f.power(), Some(true));
}
