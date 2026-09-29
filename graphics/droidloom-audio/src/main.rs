//! Host audio bridge between the Droidloom cell and the session sound server.
//!
//! Android keeps ownership of audio policy, mixing and volume. The cell's audio
//! HAL renders the primary output mix at the format the policy already declares
//! and writes those samples to the endpoint the supervisor exposes inside the
//! cell as `/dev/socket/droidloom/audio`. This service owns the host end: it
//! accepts one stream at a time and hands it to the user's `PipeWire` session.
//!
//! The cell needs no host device node, no host audio library and no privileged
//! helper, and the wire format is small enough to stay a byte pipe; see
//! `docs/contracts/audio-bridge-v1.md`.

#![forbid(unsafe_code)]

mod sink;
mod source;

use std::env;
use std::fs;
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

/// Endpoint mode. The cell-side writer runs under an Android uid and reaches
/// this inode through a bind mount, exactly like the clipboard and denial
/// endpoints, so the endpoint is writable for the cell only.
const ENDPOINT_MODE: u32 = 0o666;
/// Endpoint location below `XDG_RUNTIME_DIR`, next to the other cell endpoints.
const ENDPOINT_NAME: &str = "droidloom/audio.sock";
/// Capture endpoint location below `XDG_RUNTIME_DIR`.
const IN_ENDPOINT_NAME: &str = "droidloom/audio_in.sock";

/// Failures that stop the bridge before it serves the cell.
#[derive(Debug, thiserror::Error)]
enum AudioError {
    /// The session or the requested endpoint cannot be used as configured.
    #[error("{0}")]
    Configuration(&'static str),
    /// Socket, filesystem or notification I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("droidloom-audio: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), AudioError> {
    let runtime_root = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or(AudioError::Configuration("XDG_RUNTIME_DIR is not set"))?;
    let endpoint = env::var_os("DROIDLOOM_AUDIO_SOCKET")
        .map_or_else(|| runtime_root.join(ENDPOINT_NAME), PathBuf::from);
    let in_endpoint = env::var_os("DROIDLOOM_AUDIO_IN_SOCKET")
        .map_or_else(|| runtime_root.join(IN_ENDPOINT_NAME), PathBuf::from);
    let effective_uid = fs::metadata("/proc/self")?.uid();
    prepare_endpoint(&endpoint, &runtime_root, effective_uid)?;
    prepare_endpoint(&in_endpoint, &runtime_root, effective_uid)?;
    let listener = UnixListener::bind(&endpoint)?;
    let in_listener = UnixListener::bind(&in_endpoint)?;
    fs::set_permissions(&endpoint, fs::Permissions::from_mode(ENDPOINT_MODE))?;
    fs::set_permissions(&in_endpoint, fs::Permissions::from_mode(ENDPOINT_MODE))?;
    notify_ready()?;
    eprintln!(
        "droidloom-audio: {} listening for {} playback PCM from the cell",
        endpoint.display(),
        sink::FORMAT
    );
    eprintln!(
        "droidloom-audio: {} listening for {} capture PCM for the cell",
        in_endpoint.display(),
        source::FORMAT
    );

    std::thread::spawn(move || {
        for connection in in_listener.incoming() {
            match connection {
                Ok(stream) => serve_input(stream),
                Err(error) => eprintln!("droidloom-audio: in_accept failed: {error}"),
            }
        }
    });

    for connection in listener.incoming() {
        match connection {
            Ok(stream) => serve(stream),
            // A failed accept leaves the listener usable, so the cell keeps
            // reconnecting; audio problems never escalate to a service failure.
            Err(error) => eprintln!("droidloom-audio: accept failed: {error}"),
        }
    }
    Ok(())
}

/// Play one cell stream to completion, reporting the bytes that were forwarded.
fn serve(stream: UnixStream) {
    let opened = Instant::now();
    eprintln!("droidloom-audio: cell playback stream opened");
    match sink::play(stream) {
        Ok(bytes) => eprintln!(
            "droidloom-audio: cell playback stream ended after {bytes} bytes ({:?} of audio in {:?})",
            sink::duration_of(bytes),
            opened.elapsed()
        ),
        Err(error) => eprintln!("droidloom-audio: cell playback stream failed: {error}"),
    }
}

/// Record from the host session sound server into one cell stream to completion.
fn serve_input(stream: UnixStream) {
    let opened = Instant::now();
    eprintln!("droidloom-audio: cell capture stream opened");
    match source::record(stream) {
        Ok(bytes) => eprintln!(
            "droidloom-audio: cell capture stream ended after {bytes} bytes ({:?} of audio in {:?})",
            source::duration_of(bytes),
            opened.elapsed()
        ),
        Err(error) => eprintln!("droidloom-audio: cell capture stream failed: {error}"),
    }
}

/// Create the private runtime directory and refuse anything but our own socket.
///
/// This is the presenter's endpoint preparation, kept identical on purpose: the
/// cell endpoints share one directory, and both owners must apply the same
/// ownership, mode and replacement rules.
fn prepare_endpoint(
    endpoint: &Path,
    runtime_root: &Path,
    effective_uid: u32,
) -> Result<(), AudioError> {
    if !endpoint.is_absolute() || !runtime_root.is_absolute() {
        return Err(AudioError::Configuration(
            "runtime and endpoint paths must be absolute",
        ));
    }
    let Some(parent) = endpoint.parent() else {
        return Err(AudioError::Configuration("endpoint has no parent"));
    };
    if parent.parent() != Some(runtime_root) {
        return Err(AudioError::Configuration(
            "endpoint must be one directory below XDG_RUNTIME_DIR",
        ));
    }
    match fs::create_dir(parent) {
        Ok(()) => fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(AudioError::Configuration(
            "runtime directory is not private and session-owned",
        ));
    }
    match fs::symlink_metadata(endpoint) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(endpoint)?,
        Ok(_) => {
            return Err(AudioError::Configuration(
                "refusing to replace a non-socket endpoint",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Report readiness to systemd once the endpoint accepts connections.
fn notify_ready() -> Result<(), AudioError> {
    let Some(address) = env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let socket = UnixDatagram::unbound()?;
    let payload = b"READY=1\nSTATUS=Cell audio endpoint ready";
    if let Some(name) = address.as_bytes().strip_prefix(b"@") {
        let address = SocketAddr::from_abstract_name(name)?;
        socket.send_to_addr(payload, &address)?;
    } else {
        socket.send_to(payload, Path::new(&address))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_must_be_private_and_replace_only_a_socket() {
        let session = tempfile::tempdir().unwrap();
        let runtime = session.path().canonicalize().unwrap();
        let uid = fs::metadata("/proc/self").unwrap().uid();
        // The runtime directory is created when missing.
        let endpoint = runtime.join("droidloom/audio.sock");
        prepare_endpoint(&endpoint, &runtime, uid).unwrap();
        assert_eq!(
            fs::symlink_metadata(endpoint.parent().unwrap())
                .unwrap()
                .mode()
                & 0o777,
            0o700
        );
        // Stale sockets are replaced, other files are never touched.
        let socket = std::os::unix::net::UnixListener::bind(&endpoint).unwrap();
        drop(socket);
        prepare_endpoint(&endpoint, &runtime, uid).unwrap();
        fs::write(&endpoint, b"not a socket").unwrap();
        assert!(matches!(
            prepare_endpoint(&endpoint, &runtime, uid),
            Err(AudioError::Configuration(_))
        ));
        fs::remove_file(&endpoint).unwrap();
        // A shared or foreign runtime directory is rejected.
        fs::set_permissions(
            endpoint.parent().unwrap(),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(matches!(
            prepare_endpoint(&endpoint, &runtime, uid),
            Err(AudioError::Configuration(_))
        ));
        fs::set_permissions(
            endpoint.parent().unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert!(matches!(
            prepare_endpoint(&endpoint, &runtime, uid.wrapping_add(1)),
            Err(AudioError::Configuration(_))
        ));
        // Endpoints outside the runtime root are rejected outright.
        assert!(matches!(
            prepare_endpoint(&runtime.join("audio.sock"), &runtime, uid),
            Err(AudioError::Configuration(_))
        ));
    }
}
