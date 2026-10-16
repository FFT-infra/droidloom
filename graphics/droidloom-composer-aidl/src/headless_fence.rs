//! Preserve pending work and request a fresh completion for each empty frame.

use std::{io, os::fd::OwnedFd};

pub(crate) fn ready_fence(
    ready: Option<OwnedFd>,
    create_completed: impl FnOnce() -> io::Result<OwnedFd>,
) -> io::Result<OwnedFd> {
    if let Some(ready) = ready {
        return Ok(ready);
    }
    create_completed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    #[test]
    fn pending_fence_is_transferred_exactly_and_never_replaced_by_completed_work() {
        let (ready, mut signal) = UnixStream::pair().unwrap();
        ready.set_nonblocking(true).unwrap();
        let raw = ready.as_raw_fd();
        let fence =
            ready_fence(Some(ready.into()), || panic!("pending work must survive")).unwrap();
        assert_eq!(fence.as_raw_fd(), raw);
        let mut ready = UnixStream::from(fence);
        let mut byte = [0];
        assert_eq!(
            ready.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        signal.write_all(&[42]).unwrap();
        assert_eq!(ready.read(&mut byte).unwrap(), 1);
        assert_eq!(byte, [42]);
    }

    #[test]
    fn already_completed_frames_request_fresh_completion_each_time() {
        let mut completions = 0;
        let mut fences = Vec::new();
        for expected in 1..=128 {
            let fence = ready_fence(None, || {
                completions += 1;
                let (fd, _) = UnixStream::pair()?;
                Ok(fd.into())
            })
            .unwrap();
            assert_eq!(completions, expected);
            assert!(
                fences
                    .iter()
                    .all(|prior: &OwnedFd| prior.as_raw_fd() != fence.as_raw_fd())
            );
            fences.push(fence);
        }
    }

    #[test]
    fn completion_failure_is_propagated_and_can_be_retried() {
        assert!(ready_fence(None, || Err(io::Error::other("DRM failure"))).is_err());
        let (fd, _) = UnixStream::pair().unwrap();
        assert!(ready_fence(None, || Ok(fd.into())).is_ok());
    }
}
