//! Development-only, non-killing LMKD protocol endpoint.
//!
//! Android's activity manager requires a live LMKD control socket even when
//! memory pressure policy is delegated to the enclosing Droidloom runtime.
//! This endpoint accepts the bounded, fixed-width LMKD control protocol,
//! records no process state, never sends a signal, and returns successful
//! zero-valued replies for the three synchronous query/notification commands.

use std::env;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use std::time::Duration;

const CONTROL_SOCKET_ENV: &str = "ANDROID_SOCKET_lmkd";
const MAX_CONTROL_PACKET_BYTES: usize = 64;
const CONTROL_SOCKET_BACKLOG: libc::c_int = 8;

const LMK_GETKILLCNT: u32 = 4;
const LMK_UPDATE_PROPS: u32 = 7;
const LMK_BOOT_COMPLETED: u32 = 10;

fn reply_for(packet: &[u8]) -> Option<[u8; 8]> {
    if packet.len() < 4 || packet.len() > MAX_CONTROL_PACKET_BYTES || packet.len() % 4 != 0 {
        return None;
    }
    let command = u32::from_be_bytes(packet[..4].try_into().expect("checked command width"));
    matches!(
        (command, packet.len()),
        (LMK_GETKILLCNT, 12) | (LMK_UPDATE_PROPS | LMK_BOOT_COMPLETED, 4)
    )
    .then(|| {
        [command.to_be_bytes(), 0_u32.to_be_bytes()]
            .concat()
            .try_into()
            .unwrap()
    })
}

fn serve_client(mut client: UnixStream) -> io::Result<()> {
    let mut packet = [0_u8; MAX_CONTROL_PACKET_BYTES];
    loop {
        let length = match client.read(&mut packet) {
            Ok(0) => return Ok(()),
            Ok(length) => length,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if let Some(reply) = reply_for(&packet[..length]) {
            client.write_all(&reply)?;
        }
    }
}

fn inherited_listener() -> Result<UnixListener, String> {
    let raw_fd = env::var(CONTROL_SOCKET_ENV)
        .map_err(|_| format!("missing {CONTROL_SOCKET_ENV}"))?
        .parse::<RawFd>()
        .map_err(|error| format!("invalid {CONTROL_SOCKET_ENV}: {error}"))?;
    if raw_fd < 0 {
        return Err(format!("invalid negative {CONTROL_SOCKET_ENV}"));
    }

    // Android init creates and binds service sockets but leaves listen(2) to
    // the daemon. LMKD uses SOCK_SEQPACKET, which UnixListener can accept once
    // the inherited descriptor has entered the listening state.
    if unsafe { libc::listen(raw_fd, CONTROL_SOCKET_BACKLOG) } != 0 {
        return Err(format!(
            "listen on {CONTROL_SOCKET_ENV}: {}",
            io::Error::last_os_error()
        ));
    }

    // Android init created, bound, and exclusively transferred this descriptor
    // to the service. FromRawFd establishes the single Rust owner.
    Ok(unsafe { UnixListener::from_raw_fd(raw_fd) })
}

fn main() {
    // A client can disappear between a synchronous request and its tiny reply.
    // Ignoring SIGPIPE turns that race into the ordinary BrokenPipe error below.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let listener = inherited_listener().unwrap_or_else(|error| panic!("LMKD socket: {error}"));
    loop {
        match listener.accept() {
            Ok((client, _)) => {
                thread::spawn(move || {
                    if let Err(error) = serve_client(client) {
                        eprintln!("Droidloom LMKD client disconnected: {error}");
                    }
                });
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                eprintln!("Droidloom LMKD accept failed: {error}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synchronous_commands_receive_zero_success() {
        for command in [LMK_GETKILLCNT, LMK_UPDATE_PROPS, LMK_BOOT_COMPLETED] {
            let mut request = command.to_be_bytes().to_vec();
            if command == LMK_GETKILLCNT {
                request.extend_from_slice(&[0; 8]);
            }
            assert_eq!(
                reply_for(&request),
                Some(
                    [command.to_be_bytes(), 0_u32.to_be_bytes()]
                        .concat()
                        .try_into()
                        .unwrap()
                )
            );
        }
    }

    #[test]
    fn asynchronous_commands_and_malformed_packets_receive_no_reply() {
        assert_eq!(reply_for(&1_u32.to_be_bytes()), None);
        assert_eq!(reply_for(&[0, 0, 0]), None);
        assert_eq!(reply_for(&[0; MAX_CONTROL_PACKET_BYTES + 4]), None);
    }
}
