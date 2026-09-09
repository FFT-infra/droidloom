//! Reusable fence-availability notification for one source allocation.
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use droidloom_syncobj::{SyncobjError, WaylandTimeline};

#[derive(Default)]
pub(super) struct FenceWakeup {
    event: Option<OwnedFd>,
    unsupported: bool,
}

impl FenceWakeup {
    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.event.as_ref().map(AsFd::as_fd)
    }

    pub fn arm(&mut self, timeline: &WaylandTimeline, point: u64) -> Result<(), SyncobjError> {
        self.arm_with(|event| timeline.notify_when_available(point, event))
    }

    fn arm_with(
        &mut self,
        register: impl FnOnce(BorrowedFd<'_>) -> Result<(), SyncobjError>,
    ) -> Result<(), SyncobjError> {
        if self.unsupported {
            return Ok(());
        }
        if self.event.is_none() {
            // SAFETY: eventfd has no pointer arguments and returns an owned FD.
            let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if raw < 0 {
                return Err(wakeup_error(
                    "create fence availability eventfd",
                    io::Error::last_os_error(),
                ));
            }
            // SAFETY: the successful eventfd call transfers ownership once.
            self.event = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        }
        let event = self.event.as_ref().expect("event initialized");
        // Clear the previous point's notification before registering the next
        // one. Do not drain after registration: it may signal immediately.
        let mut value = 0_u64;
        loop {
            // SAFETY: value is eight writable bytes and event is a live FD.
            let result =
                unsafe { libc::read(event.as_raw_fd(), std::ptr::from_mut(&mut value).cast(), 8) };
            if result >= 0 {
                break;
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => {},
                io::ErrorKind::WouldBlock => break,
                _ => return Err(wakeup_error("drain fence availability eventfd", error)),
            }
        }
        match register(event.as_fd()) {
            Ok(()) => Ok(()),
            Err(SyncobjError::Drm { source, .. })
                if matches!(
                    source.raw_os_error(),
                    Some(libc::ENOTTY | libc::EOPNOTSUPP | libc::EINVAL)
                ) =>
            {
                self.unsupported = true;
                self.event = None;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

fn wakeup_error(operation: &'static str, source: io::Error) -> SyncobjError {
    SyncobjError::Drm { operation, source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Write};

    fn signal(fd: BorrowedFd<'_>) {
        File::from(fd.try_clone_to_owned().unwrap())
            .write_all(&1_u64.to_ne_bytes())
            .unwrap();
    }

    fn read(fd: BorrowedFd<'_>) -> io::Result<u64> {
        let mut value = [0; 8];
        File::from(fd.try_clone_to_owned()?).read_exact(&mut value)?;
        Ok(u64::from_ne_bytes(value))
    }

    #[test]
    fn reuse_clears_old_notifications_but_preserves_immediate_new_ones() {
        let mut wakeup = FenceWakeup::default();
        wakeup
            .arm_with(|fd| {
                signal(fd);
                Ok(())
            })
            .unwrap();
        let first = wakeup.fd().unwrap().as_raw_fd();
        let observer = wakeup.fd().unwrap().try_clone_to_owned().unwrap();
        // Leave the first point unread. Registration must see an empty counter.
        for _ in 0..128 {
            wakeup
                .arm_with(|fd| {
                    assert_eq!(fd.as_raw_fd(), first);
                    assert_eq!(read(fd).unwrap_err().kind(), io::ErrorKind::WouldBlock);
                    signal(fd);
                    Ok(())
                })
                .unwrap();
            // Observe through a descriptor retained from the first arm. Equal
            // FD numbers alone would not detect closing/recreating the object.
            assert_eq!(read(observer.as_fd()).unwrap(), 1);
            signal(observer.as_fd());
        }
        assert_eq!(read(wakeup.fd().unwrap()).unwrap(), 1);
        assert_eq!(
            read(wakeup.fd().unwrap()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn unsupported_registration_is_not_retried_on_every_frame() {
        let mut wakeup = FenceWakeup::default();
        wakeup
            .arm_with(|_| {
                Err(wakeup_error(
                    "test",
                    io::Error::from_raw_os_error(libc::ENOTTY),
                ))
            })
            .unwrap();
        assert!(wakeup.fd().is_none());
        wakeup
            .arm_with(|_| panic!("unsupported kernel must retain polling fallback"))
            .unwrap();
    }

    #[test]
    fn other_registration_failures_propagate_and_can_retry() {
        let mut wakeup = FenceWakeup::default();
        assert!(
            wakeup
                .arm_with(|_| Err(wakeup_error(
                    "test",
                    io::Error::from_raw_os_error(libc::EIO)
                )))
                .is_err()
        );
        wakeup
            .arm_with(|fd| {
                signal(fd);
                Ok(())
            })
            .unwrap();
        assert_eq!(read(wakeup.fd().unwrap()).unwrap(), 1);
    }
}
