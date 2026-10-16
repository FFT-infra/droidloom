//! Private protocol objects and scheduling tests; no compositor session or GPU.
use super::*;
use crate::{BufferId, WpPresentationFeedback};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use wayland_client::EventQueue;
use wayland_client::protocol::{wl_compositor, wl_registry};

fn grant(epoch: u64, sequence: u64, latch: u64) -> Grant {
    Grant {
        identity: Identity { epoch, sequence },
        latch_deadline_ns: latch,
        presentation_target_ns: latch + 10,
        refresh_period_ns: 10,
    }
}

#[test]
fn expired_and_regressing_grants_never_relabel_or_burst() {
    let mut schedule = Schedule::default();
    schedule.epoch(1);
    schedule.offer(grant(1, 1, 100), 50);
    assert!(schedule.consume(100).is_none());
    schedule.offer(grant(1, 1, 200), 100); // Same identity, different estimate.
    assert!(schedule.consume(100).is_none());
    schedule.offer(grant(1, 2, 200), 100);
    let consumed = schedule.consume(101).unwrap();
    assert_eq!(
        consumed.identity,
        Identity {
            epoch: 1,
            sequence: 2
        }
    );
    assert!(schedule.consume(101).is_none());
    schedule.finish(consumed.identity, Terminal::Presented);
    schedule.finish(consumed.identity, Terminal::Missed);
    assert_eq!(schedule.terminal, [1, 0, 1, 0, 0]);
}

#[test]
fn epoch_aborts_old_work_and_requires_a_fresh_grant() {
    let mut schedule = Schedule::default();
    schedule.epoch(10);
    schedule.offer(grant(10, 90, 100), 0);
    let old = schedule.consume(1).unwrap();
    schedule.epoch(11);
    schedule.finish(old.identity, Terminal::Presented); // Late feedback.
    schedule.offer(grant(10, 91, 200), 0);
    assert!(schedule.consume(1).is_none());
    schedule.offer(grant(11, 1, 200), 0);
    assert_eq!(schedule.consume(1).unwrap().identity.epoch, 11);
    assert_eq!(schedule.terminal, [0, 0, 0, 0, 1]);
}

#[test]
fn unchanged_work_and_configure_discards_have_terminal_accounting() {
    let mut schedule = Schedule::default();
    schedule.epoch(1);
    schedule.offer(grant(1, 1, 100), 0);
    schedule.offer(grant(1, 2, 200), 50); // No work used the first grant.
    let consumed = schedule.consume(51).unwrap();
    schedule.clear(Terminal::Configure);
    schedule.finish(consumed.identity, Terminal::Presented);
    assert_eq!(schedule.terminal, [0, 1, 0, 1, 0]);
    assert!(schedule.consumed.is_empty());
}

#[test]
fn portable_mode_has_no_grant_dependency_and_exact_loss_pauses() {
    let mut portable = Timeline::default();
    assert!(matches!(
        portable.reserve(10),
        Some(Reservation { target: None })
    ));
    let mut exact = Timeline {
        exact: true,
        ..Default::default()
    };
    exact.schedule.epoch(1);
    exact.schedule.offer(grant(1, 1, 100), 0);
    exact.stop();
    assert!(exact.reserve(10).is_none());
    assert!(exact.exact);
}

struct Client {
    schedule: Schedule,
}
wayland_client::delegate_noop!(Client: ignore wl_registry::WlRegistry);
wayland_client::delegate_noop!(Client: ignore wl_output::WlOutput);
wayland_client::delegate_noop!(Client: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Client: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(Client: ignore Manager);
wayland_client::delegate_noop!(Client: ignore DenialSurfaceFrameTimelineV1);
wayland_client::delegate_noop!(Client: ignore WpPresentation);
wayland_client::delegate_noop!(Client: ignore WpPresentationFeedback);
impl Dispatch<OutputTimeline, ()> for Client {
    fn event(
        state: &mut Self,
        _: &OutputTimeline,
        event: output_timeline::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            output_timeline::Event::Epoch { epoch_hi, epoch_lo } => {
                state.schedule.epoch(join(epoch_hi, epoch_lo));
            }
            output_timeline::Event::Grant {
                epoch_hi,
                epoch_lo,
                sequence_hi,
                sequence_lo,
                latch_sec_hi,
                latch_sec_lo,
                latch_nsec,
                target_sec_hi,
                target_sec_lo,
                target_nsec,
                period_ns_hi,
                period_ns_lo,
            } => {
                state.schedule.offer(
                    Grant {
                        identity: Identity {
                            epoch: join(epoch_hi, epoch_lo),
                            sequence: join(sequence_hi, sequence_lo),
                        },
                        latch_deadline_ns: presentation_timestamp_nanos(
                            join(latch_sec_hi, latch_sec_lo),
                            latch_nsec,
                        )
                        .unwrap(),
                        presentation_target_ns: presentation_timestamp_nanos(
                            join(target_sec_hi, target_sec_lo),
                            target_nsec,
                        )
                        .unwrap(),
                        refresh_period_ns: join(period_ns_hi, period_ns_lo),
                    },
                    0,
                );
            }
        }
    }
}

struct Fixture {
    conn: Connection,
    peer: UnixStream,
    queue: EventQueue<Client>,
    timeline: Timeline,
    surface: wl_surface::WlSurface,
    feedback: [WpPresentationFeedback; 2],
}
impl Fixture {
    fn new(exact: bool) -> Self {
        let (socket, peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let conn = Connection::from_socket(socket).unwrap();
        let queue = conn.new_event_queue::<Client>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let manager = exact.then(|| registry.bind::<Manager, _, _>(1, 1, &qh, ()));
        let output = registry.bind::<wl_output::WlOutput, _, _>(2, 4, &qh, ());
        let compositor = registry.bind::<wl_compositor::WlCompositor, _, _>(3, 4, &qh, ());
        let surface = compositor.create_surface(&qh, ());
        let mut timeline = Timeline::new(manager.as_ref(), Some(&surface), &qh);
        if let Some(manager) = manager {
            let proxy = manager.get_timeline(&output, &qh, ());
            proxy.set_active(1);
            timeline.subscription = Some(Subscription { output, proxy });
        }
        let presentation = registry.bind::<WpPresentation, _, _>(4, 1, &qh, ());
        let feedback = [
            presentation.feedback(&surface, &qh, ()),
            presentation.feedback(&surface, &qh, ()),
        ];
        Self {
            conn,
            peer,
            queue,
            timeline,
            surface,
            feedback,
        }
    }
    fn requests(&mut self) -> Vec<(u32, u16, Vec<u32>)> {
        self.conn.flush().unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match self.peer.read(&mut buffer) {
                Ok(0) => panic!("private peer closed"),
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        let mut records = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let word = |at| u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
            let object = word(offset);
            let header = word(offset + 4);
            let size = (header >> 16) as usize;
            assert!(size >= 8 && offset + size <= bytes.len());
            let payload = (offset + 8..offset + size).step_by(4).map(word).collect();
            records.push((object, u16::try_from(header & 0xffff).unwrap(), payload));
            offset += size;
        }
        records
    }
    fn event(&mut self, opcode: u32, payload: &[u32]) {
        let proxy = &self.timeline.subscription.as_ref().unwrap().proxy;
        let mut bytes = Vec::new();
        for word in [
            proxy.id().protocol_id(),
            (((u32::try_from(payload.len()).unwrap() + 2) * 4) << 16) | opcode,
        ]
        .into_iter()
        .chain(payload.iter().copied())
        {
            bytes.extend_from_slice(&word.to_ne_bytes());
        }
        self.peer.write_all(&bytes).unwrap();
        self.conn.prepare_read().unwrap().read().unwrap();
    }
}

#[test]
fn wire_grants_keep_their_identity_and_target_precedes_the_surface_commit() {
    let mut fixture = Fixture::new(true);
    let requests = fixture.requests();
    let output = fixture
        .timeline
        .subscription
        .as_ref()
        .unwrap()
        .proxy
        .id()
        .protocol_id();
    assert!(requests.contains(&(output, 0, vec![1]))); // set_active
    fixture.event(1, &[2, 3]);
    fixture.event(0, &[2, 3, 4, 5, 0, 42, 123, 0, 43, 456, 0, 16_666_666]);
    let mut client = Client {
        schedule: Schedule::default(),
    };
    fixture.queue.dispatch_pending(&mut client).unwrap();
    fixture.timeline.schedule = client.schedule;
    let target = fixture
        .timeline
        .reserve(42_000_000_000)
        .unwrap()
        .target
        .unwrap();
    fixture.timeline.surface.as_ref().unwrap().target(&target);
    fixture.surface.commit();
    let requests = fixture.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].1, 0); // set_target
    assert_eq!(requests[0].2, vec![output, 2, 3, 4, 5]);
    assert_eq!(requests[1], (fixture.surface.id().protocol_id(), 6, vec![]));
    assert!(fixture.timeline.reserve(42_000_000_001).is_none());
    fixture.timeline.stop();
    assert!(fixture.timeline.reserve(42_000_000_001).is_none());
    let requests = fixture.requests();
    assert!(requests.contains(&(output, 0, vec![0])));
    assert!(requests.contains(&(output, 1, vec![])));
}

#[test]
fn absent_extension_keeps_ordinary_surface_commits() {
    let mut fixture = Fixture::new(false);
    fixture.requests();
    assert!(matches!(
        fixture.timeline.reserve(1),
        Some(Reservation { target: None })
    ));
    fixture.surface.commit();
    assert_eq!(
        fixture.requests(),
        vec![(fixture.surface.id().protocol_id(), 6, vec![])]
    );
}

#[test]
fn scene_and_composed_feedback_share_idempotent_teardown_accounting() {
    let mut fixture = Fixture::new(true);
    fixture.timeline.schedule.epoch(1);
    fixture.timeline.schedule.offer(grant(1, 1, 100), 0);
    let scene = fixture.timeline.schedule.consume(1).unwrap().identity;
    fixture.timeline.schedule.offer(grant(1, 2, 200), 0);
    let composed = fixture.timeline.schedule.consume(1).unwrap().identity;
    let mut scenes = vec![SceneFeedback {
        proxy: fixture.feedback[0].clone(),
        scene: 77,
        configure_serial: 8,
        grant: Some(scene),
    }];
    let mut frames = vec![FrameFeedback {
        proxy: fixture.feedback[1].clone(),
        frame: FrameId(88),
        buffer: BufferId(99),
        submitted_nanos: 1,
        grant: Some(composed),
    }];
    let mut forgotten = Vec::new();
    retire_feedback(
        &mut fixture.timeline,
        &mut scenes,
        &mut frames,
        Terminal::Configure,
        |frame| forgotten.push(frame),
    );
    retire_feedback(
        &mut fixture.timeline,
        &mut scenes,
        &mut frames,
        Terminal::Configure,
        |frame| forgotten.push(frame),
    );
    fixture.timeline.finish(Some(scene), Terminal::Presented);
    fixture.timeline.finish(Some(composed), Terminal::Presented);
    assert!(scenes.is_empty() && frames.is_empty());
    assert_eq!(forgotten, vec![FrameId(88)]);
    assert_eq!(fixture.timeline.schedule.terminal, [0, 0, 0, 2, 0]);
}
