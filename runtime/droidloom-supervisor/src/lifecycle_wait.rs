//! Readiness sources for lifecycle requests, exact child exit and cancellation.
use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::net::UnixListener,
};

/// Sticky cancellation source. Requesting shutdown also wakes an idle daemon.
pub struct Shutdown {
    event: OwnedFd,
}
impl Shutdown {
    /// Create a nonblocking cancellation event.
    pub fn new() -> io::Result<Self> {
        // SAFETY: eventfd creates a new owned descriptor and retains no pointers.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful eventfd transferred this fresh descriptor.
        Ok(Self {
            event: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }
    /// Wake the daemon. The event remains readable until its owner drops it.
    pub fn request(&self) -> io::Result<()> {
        let value = 1_u64;
        loop {
            // SAFETY: event owns a live eventfd; value is readable for eight bytes.
            let count = unsafe { libc::write(self.as_raw_fd(), (&raw const value).cast(), 8) };
            if count == 8 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(()), // Already requested.
                _ => return Err(error),
            }
        }
    }
}
impl AsRawFd for Shutdown {
    fn as_raw_fd(&self) -> RawFd {
        self.event.as_raw_fd()
    }
}

#[derive(Default)]
pub(super) struct WaitSources {
    child: Option<(u32, OwnedFd)>,
}
impl WaitSources {
    pub fn child(&mut self, pid: Option<u32>) -> io::Result<()> {
        if self.child.as_ref().map(|(pid, _)| *pid) == pid {
            return Ok(());
        }
        self.child = None;
        if let Some(pid) = pid {
            // SAFETY: pidfd_open takes integer arguments and returns a fresh FD.
            // The daemon owns this unreaped child, so its PID cannot be reused.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful pidfd_open transferred the returned descriptor.
            self.child = Some((pid, unsafe { OwnedFd::from_raw_fd(fd as RawFd) }));
        }
        Ok(())
    }
    /// Used by readiness tests without an external descriptor.
    #[cfg(test)]
    pub fn wait(&self, listener: &UnixListener, shutdown: &Shutdown) -> io::Result<bool> {
        self.wait_with_fd(listener, shutdown, None)
    }

    /// False means shutdown. True means an FD became ready or the one-second
    /// probe interval elapsed.
    pub fn wait_with_fd(
        &self,
        listener: &UnixListener,
        shutdown: &Shutdown,
        external: Option<RawFd>,
    ) -> io::Result<bool> {
        let entry = |fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut fds = [
            entry(listener.as_raw_fd()),
            entry(shutdown.as_raw_fd()),
            entry(self.child.as_ref().map_or(-1, |(_, fd)| fd.as_raw_fd())),
            entry(external.unwrap_or(-1)),
        ];
        loop {
            // SAFETY: four initialized descriptors remain owned throughout poll.
            let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 1000) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
                return Err(io::Error::other(
                    "lifecycle readiness descriptor became invalid",
                ));
            }
            if fds[1].revents != 0 {
                return Ok(false);
            }
            if fds[0].revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                return Err(io::Error::other("lifecycle listener became unavailable"));
            }
            if result == 0 || fds[0].revents != 0 || fds[2].revents != 0 || fds[3].revents != 0 {
                return Ok(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Command, sync::Arc, thread, time::Duration};
    #[test]
    fn idle_wait_wakes_for_sticky_shutdown_without_a_client() {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("control")).unwrap();
        let shutdown = Arc::new(Shutdown::new().unwrap());
        let worker_shutdown = Arc::clone(&shutdown);
        let worker = thread::spawn(move || {
            let sources = WaitSources::default();
            assert!(!sources.wait(&listener, &worker_shutdown).unwrap());
            assert!(!sources.wait(&listener, &worker_shutdown).unwrap());
        });
        shutdown.request().unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn exact_child_exit_and_replacement_wake_the_waiter() {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("control")).unwrap();
        let shutdown = Shutdown::new().unwrap();
        let mut sources = WaitSources::default();
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        sources.child(Some(child.id())).unwrap();
        child.kill().unwrap();
        assert!(sources.wait(&listener, &shutdown).unwrap());
        child.wait().unwrap();
        sources.child(None).unwrap();
        let mut next = Command::new("sleep").arg("0.01").spawn().unwrap();
        sources.child(Some(next.id())).unwrap();
        assert!(sources.wait(&listener, &shutdown).unwrap());
        next.wait().unwrap();
        sources.child(None).unwrap();
        // Cancellation requested before entering poll must not be lost.
        thread::sleep(Duration::from_millis(1));
        shutdown.request().unwrap();
        assert!(!sources.wait(&listener, &shutdown).unwrap());
    }
}
