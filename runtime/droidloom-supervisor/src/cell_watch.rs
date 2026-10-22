//! Event-driven cell recovery companion. systemd owns retry and stop policy.
use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
};

use super::{CellState, ControlError, ControlRequest, ControlResponse, io_error, request};
use crate::boot_notice::BootNotice;

/// Start or attach to the authorized cell, follow its boot to readiness for
/// the boot notice, and wait for its exact owner to exit.
///
/// The user service stops this companion before intentionally stopping Android.
/// An unexpected exit is an error so systemd restarts the companion with a delay.
/// No polling, background thread, or periodic Android readiness probe is needed.
///
/// # Errors
/// Returns authorization/start failures, inability to watch the owner, and cell exit.
pub fn supervise_cell(socket: &Path, spec: &Path, notice: BootNotice) -> Result<(), ControlError> {
    let start = ControlRequest::Start { spec: spec.into() };
    let pid = running_pid(request(socket, &start)?)?;
    let owner = open_owner(pid).map_err(|e| io_error("watch Android cell owner", e))?;
    // The daemon may have reaped/replaced the owner between its reply and
    // pidfd_open. Never sleep on an unrelated reused PID in that case.
    if running_pid(request(socket, &start)?)? != pid {
        return Err(ControlError::Invalid(
            "Android cell changed while attaching recovery".into(),
        ));
    }
    // The daemon answers once Android is ready, or as soon as it cannot be.
    match request(socket, &ControlRequest::WaitReady) {
        Ok(response) if response.ok => notice.ready(),
        Ok(response) => notice.failed(&response.message),
        Err(error) => notice.failed(&error.to_string()),
    }
    eprintln!("watching Android cell owner {pid}; recovery is event-driven");
    wait_for_exit(&owner).map_err(|e| io_error("wait for Android cell exit", e))?;
    Err(ControlError::Invalid(format!(
        "Android cell owner {pid} exited; scheduling recovery"
    )))
}

fn running_pid(response: ControlResponse) -> Result<u32, ControlError> {
    if response.ok && response.state == CellState::Running {
        if let Some(pid) = response
            .host_pid
            .filter(|pid| *pid > 1 && *pid <= i32::MAX as u32)
        {
            return Ok(pid);
        }
    }
    Err(ControlError::Invalid(format!(
        "cannot supervise Android: {}",
        response.message
    )))
}

fn open_owner(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes scalar arguments and returns a fresh descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pidfd_open transferred ownership of this descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

fn wait_for_exit(owner: &OwnedFd) -> io::Result<()> {
    let mut fd = libc::pollfd {
        fd: owner.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: the descriptor is owned throughout poll, with one valid pollfd.
        let result = unsafe { libc::poll(&raw mut fd, 1, -1) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fd.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            return Ok(());
        }
        if fd.revents != 0 {
            return Err(io::Error::other("Android owner watch failed"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
        process::Command,
        thread,
    };

    #[test]
    fn owner_exit_wakes_without_polling_or_lifecycle_requests() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let owner = open_owner(child.id()).unwrap();
        child.kill().unwrap();
        wait_for_exit(&owner).unwrap();
        child.wait().unwrap();
        // Remains ready even after the exact owner has been reaped.
        wait_for_exit(&owner).unwrap();
    }

    #[test]
    fn recovery_attaches_to_running_cell_and_reports_its_exit() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let server = thread::spawn(move || {
            for expected in ["start", "start", "wait"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes).unwrap();
                let request = serde_json::from_slice::<ControlRequest>(&bytes).unwrap();
                assert!(match expected {
                    "start" => matches!(request, ControlRequest::Start { .. }),
                    _ => matches!(request, ControlRequest::WaitReady),
                });
                let response = serde_json::json!({"ok":true,"state":"running","message":"running","host_pid":pid});
                stream
                    .write_all(&serde_json::to_vec(&response).unwrap())
                    .unwrap();
            }
            child.kill().unwrap();
            child.wait().unwrap();
        });
        let error = supervise_cell(
            &socket,
            Path::new("/etc/droidloom/cell.json"),
            BootNotice::none(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("scheduling recovery"));
        server.join().unwrap();
    }

    #[test]
    fn failed_or_stopped_start_does_not_watch_a_pid() {
        for (ok, state, pid) in [
            (false, "running", 42),
            (true, "stopped", 42),
            (true, "running", 1),
        ] {
            let response = serde_json::from_value(
                serde_json::json!({"ok":ok,"state":state,"message":"refused","host_pid":pid}),
            )
            .unwrap();
            assert!(running_pid(response).is_err());
        }
    }
}
