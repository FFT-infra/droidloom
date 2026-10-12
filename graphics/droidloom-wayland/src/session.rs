//! One presenter owns one compositor session and the private IPC directory.
//!
//! Acquire this guard before any endpoint preparation. Keep it alive until all
//! presenter resources have stopped; unlike the updater lock it never survives exec.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};

use droidloom_denial_ipc::{PeerCredentials, peer_credentials};

/// Lifetime guard for the presenter's session-bound runtime directory.
#[derive(Debug)]
pub(crate) struct SessionLease {
    _file: File,
    runtime: PathBuf,
    peer: PeerCredentials,
    peer_start_ticks: u64,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn environment(name: &str) -> io::Result<Option<String>> {
    match env::var(name) {
        Ok(value) => {
            if value.len() > 4096 || value.chars().any(char::is_control) {
                return Err(invalid(format!("invalid {name}")));
            }
            Ok((!value.is_empty()).then_some(value))
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(invalid(format!("{name} is not UTF-8"))),
    }
}

impl SessionLease {
    /// Validate the actual compositor and acquire exclusion before touching IPC sockets.
    pub(crate) fn acquire_from_environment() -> io::Result<Self> {
        let runtime = environment("XDG_RUNTIME_DIR")?.map(PathBuf::from)
            .ok_or_else(|| invalid("XDG_RUNTIME_DIR is missing"))?;
        let display = environment("WAYLAND_DISPLAY")?
            .ok_or_else(|| invalid("WAYLAND_DISPLAY is missing; no default session is guessed"))?;
        let expected = match (
            environment("DROIDLOOM_COMPOSITOR_PID")?,
            environment("DROIDLOOM_COMPOSITOR_START_TICKS")?,
        ) {
            (None, None) => None,
            (Some(pid), Some(ticks)) => {
                let pid = pid.parse::<u32>().ok().filter(|p| *p > 0)
                    .ok_or_else(|| invalid("invalid compositor PID"))?;
                let ticks = ticks.parse::<u64>().ok().filter(|t| *t > 0)
                    .ok_or_else(|| invalid("invalid compositor start time"))?;
                Some((pid, ticks))
            }
            _ => return Err(invalid("incomplete compositor session binding")),
        };
        let endpoint = environment("DROIDLOOM_HOST_SOCKET")?.map_or_else(
            || runtime.join("droidloom/native-bridge.sock"), PathBuf::from);
        Self::acquire(runtime, &display, expected, &endpoint)
    }

    fn acquire(
        runtime: PathBuf,
        display: &str,
        expected: Option<(u32, u64)>,
        endpoint: &Path,
    ) -> io::Result<Self> {
        if !runtime.is_absolute() || runtime.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
            return Err(invalid("runtime root must be absolute and normalized"));
        }
        let uid = fs::metadata("/proc/self")?.uid();
        let meta = fs::symlink_metadata(&runtime)?;
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(invalid(format!(
                "runtime root must be private and session-owned (dir={} uid={} want_uid={} mode={:o})",
                meta.is_dir(),
                meta.uid(),
                uid,
                meta.mode() & 0o7777
            )));
        }
        let directory = runtime.join("droidloom");
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let meta = fs::symlink_metadata(&directory)?;
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(invalid("Droidloom runtime directory must be private and session-owned"));
        }
        if endpoint.parent() != Some(directory.as_path()) {
            return Err(invalid("presenter endpoint must be inside its private runtime directory"));
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join("owner.lock"))?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o077 != 0 || meta.nlink() != 1 {
            return Err(invalid("presenter lease must be a private session-owned regular file"));
        }
        // SAFETY: the owned descriptor remains live until SessionLease is dropped.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Legacy presenters do not hold owner.lock. Check kernel socket metadata
        // without connecting to or disturbing their authenticated Android endpoint.
        if let Ok(meta) = fs::symlink_metadata(endpoint) {
            if !meta.file_type().is_socket() || meta.uid() != uid {
                return Err(invalid("refusing to replace a foreign or non-socket presenter endpoint"));
            }
            if live_unix_socket(endpoint)? {
                return Err(io::Error::new(io::ErrorKind::AddrInUse,
                    "a live legacy presenter owns the native endpoint; stop that session explicitly"));
            }
        }
        if display.is_empty() || Path::new(display).components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
            return Err(invalid("invalid Wayland display path"));
        }
        let socket = if Path::new(display).is_absolute() { PathBuf::from(display) } else { runtime.join(display) };
        let meta = fs::symlink_metadata(&socket)?;
        if !meta.file_type().is_socket() || meta.uid() != uid {
            return Err(invalid("Wayland socket must be session-owned and not a symlink"));
        }
        let stream = UnixStream::connect(&socket)?;
        let peer = peer_credentials(stream.as_fd()).map_err(io::Error::other)?;
        if peer.uid != uid || peer.pid == 0 {
            return Err(invalid("Wayland peer does not belong to this desktop user"));
        }
        let peer_start_ticks = start_ticks(peer.pid)?;
        if expected.is_some_and(|identity| identity != (peer.pid, peer_start_ticks)) {
            return Err(invalid("Wayland compositor does not match the recorded session; refusing stale environment"));
        }
        Ok(Self { _file: file, runtime, peer, peer_start_ticks })
    }

    /// Check the actual Wayland connection too, closing the probe/connect race.
    pub(crate) fn verify_connection(&self, descriptor: BorrowedFd<'_>) -> io::Result<()> {
        let actual = peer_credentials(descriptor).map_err(io::Error::other)?;
        if actual != self.peer || start_ticks(actual.pid)? != self.peer_start_ticks {
            return Err(invalid("Wayland compositor changed during presenter startup"));
        }
        Ok(())
    }

    /// Validated runtime root used by the presenter's endpoint setup.
    pub(crate) fn runtime_dir(&self) -> &Path { &self.runtime }
}

fn start_ticks(pid: u32) -> io::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, rest) = stat.rsplit_once(") ").ok_or_else(|| invalid("invalid compositor process stat"))?;
    rest.split_whitespace().nth(19).ok_or_else(|| invalid("compositor start time is missing"))?
        .parse().map_err(|_| invalid("invalid compositor start time"))
}

fn live_unix_socket(path: &Path) -> io::Result<bool> {
    let expected = path.to_str().ok_or_else(|| invalid("endpoint path is not UTF-8"))?;
    let table = fs::read_to_string("/proc/net/unix")?;
    Ok(table.lines().skip(1).any(|line| {
        let mut remaining = line.trim_start();
        for _ in 0..7 {
            let Some(end) = remaining.find(char::is_whitespace) else { return false; };
            remaining = remaining[end..].trim_start();
        }
        remaining == expected
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    /// tempfile creates a 0755 directory under the usual umask; the lease
    /// requires a private 0700 root, so every fixture root is narrowed first.
    fn private_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn acquire(root: &Path) -> SessionLease {
        SessionLease::acquire(root.to_owned(), "wayland-test", None,
            &root.join("droidloom/native-bridge.sock")).unwrap()
    }

    #[test]
    fn lease_excludes_another_presenter_without_touching_its_endpoint() {
        let root = private_root();
        let _wayland = UnixListener::bind(root.path().join("wayland-test")).unwrap();
        let first = acquire(root.path());
        let error = SessionLease::acquire(root.path().to_owned(), "wayland-test", None,
            &root.path().join("droidloom/native-bridge.sock")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(!root.path().join("droidloom/native-bridge.sock").exists());
        assert_eq!(first.runtime_dir(), root.path());
        drop(first);
        assert!(root.path().join("droidloom/owner.lock").exists());
        acquire(root.path());
    }

    #[test]
    fn legacy_live_socket_is_not_mistaken_for_a_stale_file() {
        let root = private_root();
        let _wayland = UnixListener::bind(root.path().join("wayland-test")).unwrap();
        fs::DirBuilder::new().mode(0o700).create(root.path().join("droidloom")).unwrap();
        let endpoint = root.path().join("droidloom/native-bridge.sock");
        let legacy = UnixListener::bind(&endpoint).unwrap();
        assert!(live_unix_socket(&endpoint).unwrap());
        let error = SessionLease::acquire(root.path().to_owned(), "wayland-test", None, &endpoint).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(endpoint.exists());
        drop(legacy);
        assert!(!live_unix_socket(&endpoint).unwrap());
        acquire(root.path());
        // The lease leaves removal to the authenticated endpoint owner.
        assert!(endpoint.exists());
    }

    #[test]
    fn identity_checks_cover_the_actual_connection_and_start_time() {
        let root = private_root();
        let socket = root.path().join("wayland-test");
        let _wayland = UnixListener::bind(&socket).unwrap();
        let lease = acquire(root.path());
        let connection = UnixStream::connect(socket).unwrap();
        lease.verify_connection(connection.as_fd()).unwrap();
        drop(lease);
        let error = SessionLease::acquire(root.path().to_owned(), "wayland-test",
            Some((std::process::id(), 1)), &root.path().join("droidloom/native-bridge.sock"));
        assert!(error.is_err());
    }

    #[test]
    fn lease_is_not_inherited_by_exec_children() {
        let root = private_root();
        let _wayland = UnixListener::bind(root.path().join("wayland-test")).unwrap();
        let lease = acquire(root.path());
        let mut child = Command::new("sh").args(["-c", "printf ready; sleep 0.3"])
            .stdout(Stdio::piped()).spawn().unwrap();
        child.stdout.as_mut().unwrap().read_exact(&mut [0; 5]).unwrap();
        drop(lease);
        acquire(root.path());
        child.wait().unwrap();
    }

    #[test]
    fn foreign_and_symlink_runtime_paths_are_rejected() {
        let root = private_root();
        let target = tempfile::tempdir().unwrap();
        let _wayland = UnixListener::bind(root.path().join("wayland-test")).unwrap();
        std::os::unix::fs::symlink(target.path(), root.path().join("droidloom")).unwrap();
        assert!(SessionLease::acquire(root.path().to_owned(), "wayland-test", None,
            &root.path().join("droidloom/native-bridge.sock")).is_err());
        fs::remove_file(root.path().join("droidloom")).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(SessionLease::acquire(root.path().to_owned(), "wayland-test", None,
            &root.path().join("droidloom/native-bridge.sock")).is_err());
    }
}
