//! Task work selected by poll readiness or an explicit state change.
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use droidloom_denial_protocol::TaskObjectId;

pub const REQUESTS: u8 = 1;
pub const RELEASES: u8 = 2;
pub const CONFIG: u8 = 4;
pub const FRAMES: u8 = 8;
pub const UNMAP: u8 = 16;
pub const CLEANUP: u8 = 32;
pub const RESIZE: u8 = 64;
pub const TIMELINE: u8 = 128;

#[derive(Default)]
pub struct Tasks {
    ready: BTreeMap<TaskObjectId, u8>,
    pub streams: BTreeSet<TaskObjectId>,
    pub frames: BTreeSet<TaskObjectId>,
    layer_fallback: BTreeSet<TaskObjectId>,
    frame_fallback: BTreeSet<TaskObjectId>,
    fallback_deadline: Option<Instant>,
}

impl Tasks {
    pub fn mark(&mut self, object: TaskObjectId, work: u8) {
        *self.ready.entry(object).or_default() |= work;
    }

    pub fn pending(&self) -> bool {
        !self.ready.is_empty()
    }

    pub fn pop(&mut self, work: u8) -> Option<TaskObjectId> {
        let object = self
            .ready
            .iter()
            .find_map(|(object, bits)| (bits & work != 0).then_some(*object))?;
        let bits = self.ready.get_mut(&object).unwrap();
        *bits &= !work;
        if *bits == 0 {
            self.ready.remove(&object);
        }
        Some(object)
    }

    // Take a bounded turn's snapshot. Work scheduled while it is processed
    // remains pending for the next turn, rather than extending this turn.
    pub fn take(&mut self, work: u8, objects: &mut Vec<TaskObjectId>) {
        objects.clear();
        self.ready.retain(|object, bits| {
            if *bits & work != 0 {
                objects.push(*object);
                *bits &= !work;
            }
            *bits != 0
        });
    }

    pub fn remove(&mut self, object: TaskObjectId) {
        self.ready.remove(&object);
        self.streams.remove(&object);
        self.frames.remove(&object);
        self.layer_fallback.remove(&object);
        self.frame_fallback.remove(&object);
    }

    pub fn layer_fallback(&mut self, object: TaskObjectId, enabled: bool) {
        set_membership(&mut self.layer_fallback, object, enabled);
    }

    pub fn frame_fallback(&mut self, object: TaskObjectId, enabled: bool) {
        set_membership(&mut self.frame_fallback, object, enabled);
    }

    pub fn deadline(&mut self, now: Instant, interval: Duration) -> Option<Instant> {
        if self.layer_fallback.is_empty() && self.frame_fallback.is_empty() {
            self.fallback_deadline = None;
        } else if self.fallback_deadline.is_none() {
            self.fallback_deadline = Some(now + interval);
        }
        self.fallback_deadline
    }

    pub fn notify_fallback(&mut self, now: Instant) {
        if self.fallback_deadline.is_none_or(|deadline| now < deadline) {
            return;
        }
        // An absolute deadline cannot be postponed by unrelated input traffic.
        for object in &self.layer_fallback {
            *self.ready.entry(*object).or_default() |= RELEASES;
        }
        for object in &self.frame_fallback {
            *self.ready.entry(*object).or_default() |= FRAMES;
        }
        self.fallback_deadline = None;
    }
}

fn set_membership(set: &mut BTreeSet<TaskObjectId>, object: TaskObjectId, enabled: bool) {
    if enabled {
        set.insert(object);
    } else {
        set.remove(&object);
    }
}

#[derive(Clone, Copy)]
pub enum Source {
    Other,
    Task {
        object: TaskObjectId,
        work: u8,
        eventfd: bool,
    },
}

impl Source {
    pub fn notify(self, tasks: &mut Tasks, revents: i16) -> bool {
        let Self::Task {
            object,
            work,
            eventfd,
        } = self
        else {
            return false;
        };
        if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            tasks.mark(object, work);
        }
        eventfd && revents & libc::POLLIN != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

    use droidloom_denial_ipc::SeqPacket;

    #[test]
    fn kernel_readiness_selects_one_window_among_idle_streams() {
        let streams: Vec<_> = (0..64).map(|_| SeqPacket::pair().unwrap()).collect();
        let active = TaskObjectId(19);
        // SAFETY: eventfd needs no pointers and returns a new owned descriptor.
        let event = unsafe { libc::eventfd(1, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(event >= 0);
        // SAFETY: eventfd returned a new descriptor, owned only here.
        let event = unsafe { OwnedFd::from_raw_fd(event) };
        streams[19].1.send_record(b"queued scene", &[]).unwrap();
        let mut descriptors: Vec<_> = streams
            .iter()
            .map(|(requests, _)| libc::pollfd {
                fd: requests.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let mut owners: Vec<_> = (0..64)
            .map(|id| Source::Task {
                object: TaskObjectId(id),
                work: REQUESTS,
                eventfd: false,
            })
            .collect();
        descriptors.push(libc::pollfd {
            fd: event.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        owners.push(Source::Task {
            object: active,
            work: RELEASES,
            eventfd: true,
        });
        // SAFETY: the descriptors are live for the call; no pointer is retained.
        assert_eq!(
            unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, 0) },
            2
        );
        let mut tasks = Tasks::default();
        let mut drains = Vec::new();
        for (fd, owner) in descriptors.iter().zip(owners) {
            if owner.notify(&mut tasks, fd.revents) {
                drains.push(fd.fd);
            }
        }
        assert_eq!(drains, [event.as_raw_fd()]);
        assert_eq!(tasks.ready.len(), 1, "idle windows must schedule no work");
        assert_eq!(tasks.pop(RELEASES), Some(active));
        assert_eq!(tasks.pop(REQUESTS), Some(active));
        assert!(!tasks.pending());
    }

    #[test]
    fn disconnect_schedules_request_drain_without_another_write() {
        let (requests, peer) = SeqPacket::pair().unwrap();
        drop(peer);
        let mut fd = libc::pollfd {
            fd: requests.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd is writable and its socket remains live throughout poll.
        assert_eq!(unsafe { libc::poll(&raw mut fd, 1, 0) }, 1);
        assert_ne!(fd.revents & libc::POLLHUP, 0);
        let mut tasks = Tasks::default();
        assert!(
            !Source::Task {
                object: TaskObjectId(7),
                work: REQUESTS,
                eventfd: false,
            }
            .notify(&mut tasks, fd.revents)
        );
        assert_eq!(tasks.pop(REQUESTS), Some(TaskObjectId(7)));
    }

    #[test]
    fn error_readiness_schedules_owner_but_never_drains_an_unsignalled_eventfd() {
        let source = Source::Task {
            object: TaskObjectId(3),
            work: FRAMES,
            eventfd: true,
        };
        let mut tasks = Tasks::default();
        assert!(!source.notify(&mut tasks, libc::POLLOUT));
        assert!(!Source::Other.notify(&mut tasks, libc::POLLIN));
        assert!(!tasks.pending());
        for error in [libc::POLLERR, libc::POLLNVAL, libc::POLLHUP] {
            assert!(!source.notify(&mut tasks, error));
            assert_eq!(tasks.pop(FRAMES), Some(TaskObjectId(3)));
        }
    }

    #[test]
    fn bounded_request_turn_preserves_import_resume_and_cross_window_config() {
        let importing = TaskObjectId(7);
        let resized = TaskObjectId(8);
        let mut tasks = Tasks::default();
        tasks.mark(importing, REQUESTS | RELEASES);
        tasks.mark(importing, REQUESTS); // Repeated readiness coalesces.
        let mut turn = vec![TaskObjectId(99)];
        tasks.take(REQUESTS, &mut turn);
        assert_eq!(turn, [importing]);
        // Completing an import explicitly resumes its socket, while draining
        // a Composer barrier can change another window's configuration.
        tasks.mark(importing, REQUESTS);
        tasks.mark(resized, CONFIG);
        assert_eq!(tasks.pop(CONFIG), Some(resized));
        assert_eq!(tasks.pop(RELEASES), Some(importing));
        assert!(tasks.pending());
        tasks.take(REQUESTS, &mut turn);
        assert_eq!(turn, [importing]);
        assert!(!tasks.pending());
    }

    #[test]
    fn unrelated_wakeups_cannot_postpone_the_release_fallback() {
        let now = Instant::now();
        let interval = Duration::from_millis(4);
        let layer = TaskObjectId(2);
        let frame = TaskObjectId(3);
        let mut tasks = Tasks::default();
        tasks.streams.extend([layer, TaskObjectId(9)]);
        tasks.frames.extend([frame, TaskObjectId(10)]);
        tasks.layer_fallback(layer, true);
        tasks.frame_fallback(frame, true);
        for millis in 0..4 {
            let wake = now + Duration::from_millis(millis);
            assert_eq!(tasks.deadline(wake, interval), Some(now + interval));
            tasks.notify_fallback(wake);
            assert!(!tasks.pending());
        }
        tasks.notify_fallback(now + interval);
        assert_eq!(tasks.pop(RELEASES), Some(layer));
        assert_eq!(tasks.pop(FRAMES), Some(frame));
        assert!(!tasks.pending(), "eventfd-backed tasks need no timed probe");
        assert_eq!(
            tasks.deadline(now + interval, interval),
            Some(now + interval * 2)
        );
        tasks.layer_fallback(layer, false);
        tasks.frame_fallback(frame, false);
        assert_eq!(tasks.deadline(now + interval, interval), None);
    }

    #[test]
    fn removing_a_window_cancels_queued_work_and_all_fallback_watches() {
        let object = TaskObjectId(5);
        let mut tasks = Tasks::default();
        tasks.mark(object, REQUESTS | CONFIG | UNMAP | CLEANUP);
        tasks.streams.insert(object);
        tasks.frames.insert(object);
        tasks.layer_fallback(object, true);
        tasks.frame_fallback(object, true);
        let now = Instant::now();
        let interval = Duration::from_millis(4);
        assert_eq!(tasks.deadline(now, interval), Some(now + interval));
        tasks.remove(object);
        tasks.notify_fallback(now + interval);
        assert!(!tasks.pending());
        assert!(tasks.streams.is_empty());
        assert!(tasks.frames.is_empty());
        assert_eq!(tasks.deadline(now + interval, interval), None);
    }
}
