//! Session-owned camera bridge. Android keeps camera permission and HAL policy.

#![forbid(unsafe_code)]

mod capture;
mod protocol;

use std::fs;
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use clap::Parser;
use protocol::{Kind, Request, Status};

const MAX_CONNECTIONS: usize = 4;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const ANDROID_SYSTEM_UID: u32 = 1000;

#[derive(Parser)]
#[command(
    version,
    about = "Bridge session cameras to the Android camera producer"
)]
struct Args {
    /// Check `PipeWire` camera discovery without opening a sensor or socket.
    #[arg(long)]
    check: bool,
}

fn main() -> ExitCode {
    match run(&Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("droidloom-camera: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> io::Result<()> {
    gstreamer::init().map_err(io::Error::other)?;
    if args.check {
        let sources = capture::sources()?;
        println!(
            "camera sources: rear={}, front={}",
            sources[0].is_some(),
            sources[1].is_some()
        );
        return if sources.iter().any(Option::is_some) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no supported camera source",
            ))
        };
    }
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    let uid = fs::metadata("/proc/self")?.uid();
    let endpoint = root.join("droidloom/camera.sock");
    let listener = open_listener(&endpoint, &root, uid, droidloom_listenfd::inherited()?)?;
    notify_ready()?;
    let capture = Arc::new(CaptureSlot::default());
    let connections = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let mut stream = stream?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let peer = rustix::net::sockopt::socket_peercred(&stream)?;
        if !trusted_uid(peer.uid.as_raw(), uid) {
            eprintln!(
                "droidloom-camera: rejected untrusted peer uid {}",
                peer.uid.as_raw()
            );
            continue;
        }
        let Some(slot) = Connection::claim(&connections) else {
            let _ = protocol::write_packet(&mut stream, Kind::Error, Status::Busy, 0, &[]);
            continue;
        };
        let capture = Arc::clone(&capture);
        std::thread::Builder::new()
            .name("camera-client".into())
            .spawn(move || {
                let _slot = slot;
                if let Err(error) = serve(&mut stream, &capture) {
                    let status = match error.kind() {
                        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => {
                            Status::Protocol
                        }
                        io::ErrorKind::NotFound => Status::Unavailable,
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Status::Timeout,
                        _ => Status::Capture,
                    };
                    let _ = protocol::write_packet(&mut stream, Kind::Error, status, 0, &[]);
                    eprintln!("droidloom-camera: stream ended: {error}");
                }
            })?;
    }
    Ok(())
}

fn trusted_uid(peer: u32, owner: u32) -> bool {
    peer == owner || peer == ANDROID_SYSTEM_UID
}

struct Connection(Arc<AtomicUsize>);

impl Connection {
    fn claim(counter: &Arc<AtomicUsize>) -> Option<Self> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_CONNECTIONS).then_some(n + 1)
            })
            .ok()
            .map(|_| Self(Arc::clone(counter)))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
struct CaptureSlot {
    active: Mutex<bool>,
    stopped: Condvar,
}

struct CaptureLease<'a>(&'a CaptureSlot);

impl CaptureSlot {
    fn lease(
        &self,
        timeout: Duration,
        stream: &UnixStream,
    ) -> io::Result<Option<CaptureLease<'_>>> {
        let deadline = Instant::now() + timeout;
        let mut active = self
            .active
            .lock()
            .map_err(|_| io::Error::other("camera lifecycle lock is poisoned"))?;
        // Closing the Android socket precedes host pipeline teardown. Wait for
        // teardown without allowing a cancelled open to acquire the next lease.
        loop {
            capture::ensure_consumer_connected(stream)?;
            if !*active {
                *active = true;
                return Ok(Some(CaptureLease(self)));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            (active, _) = self
                .stopped
                .wait_timeout(active, remaining.min(Duration::from_millis(100)))
                .map_err(|_| io::Error::other("camera lifecycle lock is poisoned"))?;
        }
    }
}

impl Drop for CaptureLease<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.0.active.lock() {
            *active = false;
            self.0.stopped.notify_one();
        }
    }
}

fn serve(stream: &mut UnixStream, capture: &CaptureSlot) -> io::Result<()> {
    match protocol::read_request(stream)? {
        Request::Catalogue => {
            let sources = capture::sources()?;
            let mask = u32::from(sources[0].is_some()) | (u32::from(sources[1].is_some()) << 1);
            protocol::write_packet(stream, Kind::Catalogue, Status::Ok, 0, &mask.to_le_bytes())
        }
        Request::Open(id) => {
            let Some(_lease) = capture.lease(Duration::from_secs(3), stream)? else {
                return protocol::write_packet(stream, Kind::Error, Status::Busy, 0, &[]);
            };
            let sources = capture::sources()?;
            let source = sources[id as usize].as_deref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "requested camera is unavailable")
            })?;
            capture::stream(stream, source)
        }
    }
}

fn open_listener(
    endpoint: &Path,
    root: &Path,
    uid: u32,
    inherited: Option<UnixListener>,
) -> io::Result<UnixListener> {
    if let Some(listener) = inherited {
        prepare_directory(endpoint, root, uid)?;
        let metadata = fs::symlink_metadata(endpoint)?;
        if listener.local_addr()?.as_pathname() != Some(endpoint)
            || !metadata.file_type().is_socket()
            || metadata.uid() != uid
        {
            return Err(io::Error::other(
                "activation socket does not match the camera endpoint",
            ));
        }
        // systemd owns this listener across service restarts; never unlink it.
        Ok(listener)
    } else {
        prepare_endpoint(endpoint, root, uid)?;
        let listener = UnixListener::bind(endpoint)?;
        fs::set_permissions(endpoint, fs::Permissions::from_mode(0o666))?;
        Ok(listener)
    }
}

fn prepare_directory(endpoint: &Path, root: &Path, uid: u32) -> io::Result<()> {
    let parent = endpoint
        .parent()
        .ok_or_else(|| io::Error::other("endpoint has no parent"))?;
    if !root.is_absolute() || parent.parent() != Some(root) {
        return Err(io::Error::other(
            "camera endpoint must be below XDG_RUNTIME_DIR/droidloom",
        ));
    }
    match fs::create_dir(parent) {
        Ok(()) => fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "camera runtime directory is not private and session-owned",
        ));
    }
    Ok(())
}

fn prepare_endpoint(endpoint: &Path, root: &Path, uid: u32) -> io::Result<()> {
    prepare_directory(endpoint, root, uid)?;
    match fs::symlink_metadata(endpoint) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() || metadata.uid() != uid {
                return Err(io::Error::other(
                    "refusing to replace a non-owned camera socket",
                ));
            }
            match UnixStream::connect(endpoint) {
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(endpoint)
                }
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "camera bridge is already running",
                )),
                Err(error) => Err(error),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn notify_ready() -> io::Result<()> {
    let Some(address) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let socket = UnixDatagram::unbound()?;
    let payload = b"READY=1\nSTATUS=Android camera bridge listening";
    if let Some(name) = address.as_bytes().strip_prefix(b"@") {
        socket.send_to_addr(payload, &SocketAddr::from_abstract_name(name)?)?;
    } else {
        socket.send_to(payload, Path::new(&address))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_socket_excludes_android_application_uids() {
        assert!(trusted_uid(1000, 1000));
        assert!(trusted_uid(1000, 1001));
        assert!(trusted_uid(1001, 1001));
        assert!(!trusted_uid(10_000, 1000));
        assert!(!trusted_uid(10_047, 1000));
        assert!(!trusted_uid(2000, 1000));
    }

    #[test]
    fn connections_are_bounded_and_slots_return_on_drop() {
        let counter = Arc::new(AtomicUsize::new(0));
        let slots: Vec<_> = (0..MAX_CONNECTIONS)
            .map(|_| Connection::claim(&counter).unwrap())
            .collect();
        assert!(Connection::claim(&counter).is_none());
        drop(slots);
        assert_eq!(counter.load(Ordering::Acquire), 0);
        assert!(Connection::claim(&counter).is_some());
    }

    #[test]
    fn immediate_reopen_waits_for_previous_pipeline_teardown() {
        let slot = Arc::new(CaptureSlot::default());
        let (previous_server, _previous_client) = UnixStream::pair().unwrap();
        let previous = slot
            .lease(Duration::ZERO, &previous_server)
            .unwrap()
            .unwrap();
        let (server, _client) = UnixStream::pair().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let next = Arc::clone(&slot);
        let worker = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            let lease = next.lease(Duration::from_secs(2), &server).unwrap();
            result_tx.send(lease.is_some()).unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let during_teardown = result_rx.recv_timeout(Duration::from_millis(100));
        drop(previous);
        worker.join().unwrap();
        assert!(
            matches!(
                during_teardown,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "new camera was rejected before old teardown completed: {during_teardown:?}"
        );
        assert!(result_rx.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn independently_active_camera_stays_busy_after_bounded_wait() {
        let slot = CaptureSlot::default();
        let (server, _client) = UnixStream::pair().unwrap();
        let _active = slot.lease(Duration::ZERO, &server).unwrap().unwrap();
        assert!(
            slot.lease(Duration::from_millis(10), &server)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn queued_open_cancellation_never_acquires_capture_lease() {
        let slot = Arc::new(CaptureSlot::default());
        let (previous_server, _previous_client) = UnixStream::pair().unwrap();
        let previous = slot
            .lease(Duration::ZERO, &previous_server)
            .unwrap()
            .unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let starts = Arc::new(AtomicUsize::new(0));
        let next = Arc::clone(&slot);
        let backend_starts = Arc::clone(&starts);
        let worker = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            let result = match next.lease(Duration::from_secs(3), &server) {
                Ok(Some(_lease)) => {
                    backend_starts.fetch_add(1, Ordering::SeqCst);
                    Ok(true)
                }
                Ok(None) => Ok(false),
                Err(error) => Err(error.kind()),
            };
            result_tx.send(result).unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let waiting = result_rx.recv_timeout(Duration::from_millis(50));
        client.shutdown(std::net::Shutdown::Both).unwrap();
        // Cancellation must finish while the previous camera still owns its lease.
        let cancelled = result_rx.recv_timeout(Duration::from_secs(1));
        drop(previous);
        worker.join().unwrap();
        assert!(matches!(
            waiting,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(
            matches!(cancelled, Ok(Err(io::ErrorKind::ConnectionAborted)))
                && starts.load(Ordering::SeqCst) == 0,
            "cancelled open reached capture: result={cancelled:?}, starts={}",
            starts.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn inherited_listener_keeps_the_cell_socket_inode_across_service_restarts() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let path = root.join("droidloom/camera.sock");
        let socket_unit = open_listener(&path, &root, uid, None).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        for _ in 0..2 {
            let service =
                open_listener(&path, &root, uid, Some(socket_unit.try_clone().unwrap())).unwrap();
            let _client = UnixStream::connect(&path).unwrap();
            let _accepted = service.accept().unwrap();
            drop(service);
            assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        }
        let wrong = UnixListener::bind(root.join("different.sock")).unwrap();
        assert!(open_listener(&path, &root, uid, Some(wrong)).is_err());
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    }

    #[test]
    fn endpoint_refuses_live_sockets_and_non_socket_files() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let path = root.join("droidloom/camera.sock");
        prepare_endpoint(&path, &root, uid).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        assert_eq!(
            prepare_endpoint(&path, &root, uid).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
        drop(listener);
        prepare_endpoint(&path, &root, uid).unwrap();
        fs::write(&path, b"keep").unwrap();
        assert!(prepare_endpoint(&path, &root, uid).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        fs::remove_file(&path).unwrap();
        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prepare_endpoint(&path, &root, uid).is_err());
    }
}
