//! Inheritance of a systemd socket-activated listening socket.
//!
//! The socket unit owns the endpoint file; the service only borrows the
//! already-bound, already-listening descriptor from environment file
//! descriptor 3. Preserving that inode across service restarts is what lets
//! a cell's bind mount stay valid without restarting the cell. This crate
//! exists on its own so every consumer keeps `unsafe_code = "forbid"` and
//! the single descriptor adoption below stays reviewable in one place.

use std::env;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixListener;

/// The first descriptor systemd passes to socket-activated services.
const LISTEN_FDS_START: i32 = 3;

/// Take the inherited listener if the unit started this service by socket
/// activation. No descriptor means the service binds its own socket.
///
/// # Errors
///
/// Returns when the activation environment names this process but the
/// descriptor is absent, is not a listening Unix stream socket, or the
/// protocol asks for more than the single endpoint this service serves.
pub fn inherited() -> io::Result<Option<UnixListener>> {
    let Some(count) = activation_count()? else {
        return Ok(None);
    };
    if count != 1 {
        return Err(io::Error::other(
            "camera service expects exactly one activation socket",
        ));
    }
    // SAFETY: LISTEN_FDS == 1 is systemd's contract that descriptor 3 is this
    // unit's listening socket, created before the process started. Ownership
    // of that descriptor is transferred into the returned listener; every use
    // after this line goes through the safe UnixListener API.
    let listener = unsafe { UnixListener::from_raw_fd(LISTEN_FDS_START) };
    validate(&listener)?;
    set_cloexec()?;
    Ok(Some(listener))
}

/// Number of descriptors declared for this exact process, if any.
fn activation_count() -> io::Result<Option<u32>> {
    if env::var_os("LISTEN_PID").is_none() || env::var_os("LISTEN_FDS").is_none() {
        return Ok(None);
    }
    let pid: u32 = env::var("LISTEN_PID")
        .map_err(|_| io::Error::other("LISTEN_PID is not valid UTF-8"))?
        .parse()
        .map_err(|_| io::Error::other("LISTEN_PID is not a process id"))?;
    if pid != std::process::id() {
        // The environment was meant for a different process.
        return Ok(None);
    }
    let count: u32 = env::var("LISTEN_FDS")
        .map_err(|_| io::Error::other("LISTEN_FDS is not valid UTF-8"))?
        .parse()
        .map_err(|_| io::Error::other("LISTEN_FDS is not a count"))?;
    Ok(Some(count))
}

/// Read one integer socket option.
fn option(fd: i32, name: i32) -> io::Result<i32> {
    let mut value: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: value and length point at a c_int and its size, the layout
    // getsockopt expects for an integer option.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            name,
            (&raw mut value).cast(),
            &mut length,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

/// The descriptor must be a Unix stream socket that is already listening.
fn validate(listener: &UnixListener) -> io::Result<()> {
    let fd = listener.as_raw_fd();
    if option(fd, libc::SO_DOMAIN)? != libc::AF_UNIX
        || option(fd, libc::SO_TYPE)? != libc::SOCK_STREAM
        || option(fd, libc::SO_ACCEPTCONN)? != 1
    {
        return Err(io::Error::other(
            "activation descriptor is not a listening Unix stream socket",
        ));
    }
    Ok(())
}

fn set_cloexec() -> io::Result<()> {
    // SAFETY: F_GETFD/F_SETFD only read and update the close-on-exec flag of
    // the inherited descriptor; no pointer is involved.
    let flags = unsafe { libc::fcntl(LISTEN_FDS_START, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::FD_CLOEXEC == 0 {
        // SAFETY: same as above.
        if unsafe { libc::fcntl(LISTEN_FDS_START, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_environment_never_borrows_a_descriptor() {
        // Tests run without activation variables; nothing may be claimed.
        assert!(env::var_os("LISTEN_FDS").is_none());
        assert!(inherited().unwrap().is_none());
    }
}
