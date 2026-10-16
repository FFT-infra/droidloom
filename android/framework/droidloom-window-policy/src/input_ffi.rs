//! Owned FDs and Binder-token handles at the native `InputFlinger` boundary.
use crate::input::{Engine, Event, RoutedEvent, Target};
use droidloom_denial_ipc::{IpcError, SeqPacket, SeqPacketListener};
use droidloom_input_protocol::{RECORD_BYTES, Record, decode};
use std::{
    collections::BTreeMap,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    thread::JoinHandle,
};

#[unsafe(no_mangle)]
pub static DROIDLOOM_NATIVE_INPUT_ABI: [u8; 29] = *b"DROIDLOOM_NATIVE_INPUT_ABI=2;";
#[unsafe(no_mangle)]
pub static DROIDLOOM_NATIVE_INPUT_WIRE_ABI: [u8; 26] =
    droidloom_input_protocol::BUILD_COMPATIBILITY;

type Inject = unsafe extern "C" fn(usize, *const Event, usize) -> bool;
type WorkerLifecycle = unsafe extern "C" fn(usize, bool) -> bool;
type Release = unsafe extern "C" fn(usize);
#[derive(Clone, Copy)]
struct Sink {
    context: usize,
    inject: Inject,
    lifecycle: WorkerLifecycle,
}
struct Token {
    pointer: usize,
    incarnation: u64,
    release: Release,
}
impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: the C++ publisher transfers one owned sp<IBinder> allocation.
        unsafe { (self.release)(self.pointer) };
    }
}
#[derive(Default)]
struct State {
    engine: Engine,
    tokens: BTreeMap<i32, Token>,
    sink: Option<Sink>,
}
static STATE: OnceLock<Mutex<State>> = OnceLock::new();
fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .get_or_init(|| Mutex::new(State::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn now() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time is a writable stack timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut time) } != 0 {
        return 0;
    }
    u64::try_from(time.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(time.tv_nsec).unwrap_or(0))
}
impl State {
    fn dispatch(&self, event: &Event, token: usize) {
        if let Some(sink) = self.sink {
            // SAFETY: C++ keeps its context alive until stop joins the worker;
            // the state lock retains the token allocation throughout this call.
            if !unsafe { (sink.inject)(sink.context, event, token) } {
                eprintln!(
                    "Droidloom native input dispatch rejected kind {}",
                    event.kind
                );
            }
        }
    }
    fn dispatch_routed(&self, event: &RoutedEvent) {
        if let Some(token) = self.tokens.get(&event.target.task) {
            self.dispatch(&event.event, token.pointer);
        }
    }
    fn reset(&mut self) {
        for event in self.engine.reset(now()) {
            self.dispatch_routed(&event);
        }
    }
}

/// The caller transfers token ownership even when validation rejects the route.
/// Publication must run outside WMS's global lock, as cancellation dispatch can
/// invoke Android input policy synchronously.
#[unsafe(no_mangle)]
pub extern "C" fn droidloom_input_publish(
    task: i32,
    display: i32,
    incarnation: i64,
    left: i32,
    top: i32,
    token_identity: u64,
    token: usize,
    release: Release,
) -> bool {
    if token == 0 {
        return false;
    }
    let token = Token {
        pointer: token,
        incarnation: u64::try_from(incarnation).unwrap_or(0),
        release,
    };
    if task <= 0 || display < 0 || incarnation <= 0 || token_identity == 0 {
        return false;
    }
    let target = Target {
        task,
        display,
        incarnation: token.incarnation,
        token: token_identity,
        left,
        top,
    };
    let mut state = state();
    for event in state.engine.publish(target, now()) {
        state.dispatch_routed(&event);
    }
    state.tokens.insert(task, token);
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn droidloom_input_retire(task: i32, incarnation: i64) {
    let Ok(incarnation) = u64::try_from(incarnation) else {
        return;
    };
    let mut state = state();
    if incarnation == 0
        || state
            .tokens
            .get(&task)
            .is_none_or(|token| token.incarnation != incarnation)
    {
        return;
    }
    for event in state.engine.retire(task, incarnation, now()) {
        state.dispatch_routed(&event);
    }
    state.tokens.remove(&task);
}

pub struct Handle {
    worker: Mutex<Option<Worker>>,
    sink: Sink,
}
struct Worker {
    wake: SeqPacket,
    thread: JoinHandle<()>,
    _path: Option<SocketPath>,
}
struct SocketPath {
    path: PathBuf,
    inode: u64,
}
impl Drop for SocketPath {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.ino() == self.inode && m.file_type().is_socket())
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
impl Worker {
    fn stop(self) {
        let _ = self.wake.send_record(b"stop", &[]);
        let _ = self.thread.join();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn droidloom_input_create(
    context: usize,
    inject: Inject,
    lifecycle: WorkerLifecycle,
) -> *mut Handle {
    let sink = Sink {
        context,
        inject,
        lifecycle,
    };
    state().sink = Some(sink);
    Box::into_raw(Box::new(Handle {
        worker: Mutex::new(None),
        sink,
    }))
}

/// # Safety
/// `handle` must be an exclusively managed live pointer returned by create.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn droidloom_input_listen(handle: *mut Handle, fd: i32) -> bool {
    if handle.is_null() || fd < 0 {
        return false;
    }
    // SAFETY: C++ supplies a live Handle and a borrowed Binder parcel FD.
    let handle = unsafe { &*handle };
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return false;
    }
    // SAFETY: duplicate is a newly owned descriptor, closed on all error paths.
    let fd = unsafe { OwnedFd::from_raw_fd(duplicate) };
    let Ok(listener) = SeqPacketListener::from_owned_fd(fd) else {
        return false;
    };
    listen(handle, listener, None)
}

/// # Safety
/// `handle` must be an exclusively managed live pointer returned by create.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn droidloom_input_start(handle: *mut Handle) -> bool {
    if handle.is_null() {
        return false;
    }
    // SAFETY: the C++ registration lock owns this live handle.
    start_at(
        unsafe { &*handle },
        Path::new("/dev/socket/droidloom-framework/input"),
    )
}

fn start_at(handle: &Handle, path: &Path) -> bool {
    // The containing directory is private to Android system/root. Never replace
    // a symlink, ordinary file, or socket belonging to another principal.
    if let Some(previous) = handle
        .worker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        previous.stop();
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() } => {
            if std::fs::remove_file(path).is_err() {
                return false;
            }
        }
        Ok(_) => return false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return false,
    }
    let Ok(listener) = SeqPacketListener::bind(path) else {
        return false;
    };
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let owner = SocketPath {
        path: path.into(),
        inode: metadata.ino(),
    };
    if std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660)).is_err() {
        return false;
    }
    listen(handle, listener, Some(owner))
}

fn listen(handle: &Handle, listener: SeqPacketListener, path: Option<SocketPath>) -> bool {
    if listener.set_nonblocking(true).is_err() {
        return false;
    }
    let Ok((wake, receiver)) = SeqPacket::pair() else {
        return false;
    };
    let mut worker = handle
        .worker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(previous) = worker.take() {
        previous.stop();
    }
    state().sink = Some(handle.sink);
    let sink = handle.sink;
    let (started, startup) = std::sync::mpsc::sync_channel(1);
    let Ok(thread) = std::thread::Builder::new()
        .name("DroidloomInput".into())
        .spawn(move || {
            // SAFETY: the callback context remains live until stop joins us.
            // Android attaches this thread once for its existing policy hooks.
            let attached = unsafe { (sink.lifecycle)(sink.context, true) };
            let _ = started.send(attached);
            if !attached {
                return;
            }
            if let Err(error) = receive(&listener, &receiver) {
                eprintln!("Droidloom native input: {error}");
            }
            state().reset();
            // SAFETY: cancellation has finished; this is the matching detach.
            unsafe { (sink.lifecycle)(sink.context, false) };
        })
    else {
        return false;
    };
    if !startup.recv().unwrap_or(false) {
        let _ = thread.join();
        return false;
    }
    *worker = Some(Worker {
        wake,
        thread,
        _path: path,
    });
    true
}

/// # Safety
/// `handle` must be a unique pointer from create, with no concurrent listen call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn droidloom_input_destroy(handle: *mut Handle) {
    if handle.is_null() {
        return;
    }
    // SAFETY: C++ destroys the allocation once, before freeing the callback context.
    let handle = unsafe { Box::from_raw(handle) };
    if let Some(worker) = handle
        .worker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        worker.stop();
    }
    let mut state = state();
    state.reset();
    state.sink = None;
    state.engine = Engine::default();
    state.tokens.clear();
}

fn ready(fd: std::os::fd::BorrowedFd<'_>, wake: &SeqPacket) -> Result<bool, String> {
    let mut fds = [
        libc::pollfd {
            fd: wake.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: poll receives the exact live two-element array; no timeout poll.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.to_string());
        }
        return Ok(fds[0].revents == 0);
    }
}

fn receive(listener: &SeqPacketListener, wake: &SeqPacket) -> Result<(), String> {
    while ready(listener.as_fd(), wake)? {
        let client = match listener.accept() {
            Ok(client) => client,
            Err(IpcError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        let credentials = client
            .peer_credentials()
            .map_err(|error| error.to_string())?;
        // The in-process listener fixture runs under the developer's host UID;
        // production Android still accepts only root or system.
        #[cfg(test)]
        let fixture_peer = credentials.uid == unsafe { libc::geteuid() };
        #[cfg(not(test))]
        let fixture_peer = false;
        if credentials.uid != 0 && credentials.uid != 1000 && !fixture_peer {
            continue;
        }
        state().reset();
        let mut bytes = [0; RECORD_BYTES];
        while ready(client.as_fd(), wake)? {
            let record = match client.receive_record_into(&mut bytes) {
                Ok(record) if record.descriptors.is_empty() => record,
                _ => break,
            };
            let Ok(decoded) = decode(record.bytes) else {
                break;
            };
            let mut state = state();
            match decoded {
                Record::DisplayPower(powered) => {
                    if !powered {
                        state.reset();
                    }
                    state.dispatch(
                        &Event {
                            kind: 6,
                            action: i32::from(powered),
                            ..Event::default()
                        },
                        0,
                    );
                }
                Record::Input(sample) => match state.engine.process_batch(sample) {
                    Ok(events) => {
                        for event in events {
                            state.dispatch_routed(&event);
                        }
                    }
                    Err(error) => {
                        eprintln!("Droidloom native input sample rejected: {error}");
                        state.reset();
                    }
                },
            }
        }
        state().reset();
        // If shutdown woke the inner loop, it must also end the accept loop.
        if !ready_now(wake)? {
            return Ok(());
        }
    }
    Ok(())
}

fn ready_now(wake: &SeqPacket) -> Result<bool, String> {
    let mut fd = libc::pollfd {
        fd: wake.as_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: single initialized pollfd; zero timeout only checks shutdown after disconnect.
    if unsafe { libc::poll(&raw mut fd, 1, 0) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(fd.revents == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use droidloom_input_protocol::{encode_key, encode_touch};
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };

    struct Context {
        sender: mpsc::Sender<Event>,
        starts: AtomicUsize,
        stops: AtomicUsize,
    }
    unsafe extern "C" fn lifecycle(context: usize, starting: bool) -> bool {
        // SAFETY: the test retains the callback context until both workers join.
        let context = unsafe { &*(context as *const Context) };
        if starting {
            context.starts.fetch_add(1, Ordering::SeqCst);
        } else {
            context.stops.fetch_add(1, Ordering::SeqCst);
        }
        true
    }
    unsafe extern "C" fn startup_failure(_: usize, _: bool) -> bool {
        false
    }

    unsafe extern "C" fn inject(context: usize, event: *const Event, _: usize) -> bool {
        // SAFETY: test owns both allocations until the receiver is joined.
        let context = unsafe { &*(context as *const Context) };
        context.sender.send(unsafe { *event }).is_ok()
    }
    unsafe extern "C" fn release(token: usize) {
        // SAFETY: publish transfers exactly one Box allocated by this test.
        drop(unsafe { Box::from_raw(token as *mut u64) });
    }
    #[test]
    #[allow(clippy::too_many_lines)] // Exercise the full reconnect/retire/shutdown lifecycle.
    fn inherited_socket_dispatches_cancels_and_replaces_without_an_idle_poll() {
        assert_eq!(std::mem::size_of::<Event>(), 576);
        let dir = tempfile::tempdir().unwrap();
        let listener = SeqPacketListener::bind(dir.path().join("input")).unwrap();
        let (sender, received) = mpsc::channel::<Event>();
        let context = Box::into_raw(Box::new(Context {
            sender,
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        }));
        let handle = droidloom_input_create(context as usize, inject, lifecycle);
        assert!(droidloom_input_publish(
            42,
            0,
            7,
            30,
            40,
            9,
            Box::into_raw(Box::new(9_u64)) as usize,
            release
        ));
        // SAFETY: live Handle and listener; destroy is called once after all I/O.
        assert!(unsafe { droidloom_input_listen(handle, listener.as_fd().as_raw_fd()) });
        let client = SeqPacket::connect(dir.path().join("input")).unwrap();
        for (action, button, expected) in [
            (0, 0, &[9][..]),
            (3, 0x111, &[0, 11][..]),
            (5, 0, &[8][..]),
            (4, 0x111, &[12, 1][..]),
            (2, 0, &[10][..]),
        ] {
            let scroll = if action == 5 { -128 } else { 0 };
            client
                .send_record(
                    &droidloom_input_protocol::encode_mouse(
                        0,
                        42,
                        90,
                        action,
                        button,
                        2 << 16,
                        3 << 16,
                        0,
                        scroll,
                        7,
                    )
                    .unwrap(),
                    &[],
                )
                .unwrap();
            for expected_action in expected {
                let mouse = received.recv_timeout(Duration::from_secs(2)).unwrap();
                assert_eq!(
                    (mouse.kind, mouse.action, mouse.count),
                    (7, *expected_action, 1)
                );
                assert_eq!((mouse.pointers[0].x, mouse.pointers[0].y), (32.0, 43.0));
                if action == 5 {
                    assert_eq!(mouse.pointers[1].y, -0.5);
                }
            }
        }
        client
            .send_record(
                &encode_touch(0, 42, 100, 0, 0, 2 << 16, 3 << 16, u16::MAX, 7).unwrap(),
                &[],
            )
            .unwrap();
        let event = received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!((event.pointers[0].x, event.pointers[0].y), (32.0, 43.0));
        client
            .send_record(&encode_key(0, 42, 101, 0, 30, 0, 55, 7).unwrap(), &[])
            .unwrap();
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .serial,
            55
        );
        droidloom_input_retire(42, 7);
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .action,
            3
        );
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap().flags & 0x20,
            0x20
        );
        assert!(droidloom_input_publish(
            42,
            0,
            8,
            0,
            0,
            10,
            Box::into_raw(Box::new(10_u64)) as usize,
            release
        ));
        client
            .send_record(&encode_key(0, 42, 102, 0, 30, 0, 56, 7).unwrap(), &[])
            .unwrap();
        client
            .send_record(&encode_key(0, 42, 103, 0, 30, 0, 57, 8).unwrap(), &[])
            .unwrap();
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .serial,
            57
        );
        let replacement = SeqPacketListener::bind(dir.path().join("replacement")).unwrap();
        // SAFETY: same live Handle, replacement FD remains borrowed.
        assert!(unsafe { droidloom_input_listen(handle, replacement.as_fd().as_raw_fd()) });
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap().flags & 0x20,
            0x20
        );
        let replacement_client = SeqPacket::connect(dir.path().join("replacement")).unwrap();
        replacement_client
            .send_record(&droidloom_input_protocol::encode_display_power(false), &[])
            .unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap().kind,
            6
        );
        droidloom_input_retire(42, 8);
        let owned_path = dir.path().join("owned");
        // SAFETY: the live handle stays exclusively owned by this test.
        assert!(start_at(unsafe { &*handle }, &owned_path));
        assert_eq!(
            std::fs::metadata(&owned_path).unwrap().permissions().mode() & 0o777,
            0o660
        );
        let owned_client = SeqPacket::connect(&owned_path).unwrap();
        owned_client
            .send_record(&droidloom_input_protocol::encode_display_power(true), &[])
            .unwrap();
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .action,
            1
        );
        // Restart without an init-owned FD, including cleanup of the old listener.
        assert!(start_at(unsafe { &*handle }, &owned_path));
        let reconnected = SeqPacket::connect(&owned_path).unwrap();
        reconnected
            .send_record(&droidloom_input_protocol::encode_display_power(false), &[])
            .unwrap();
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .action,
            0
        );
        let ordinary_file = dir.path().join("ordinary");
        std::fs::write(&ordinary_file, b"preserve").unwrap();
        assert!(!start_at(unsafe { &*handle }, &ordinary_file));
        assert_eq!(std::fs::read(&ordinary_file).unwrap(), b"preserve");
        let symlink = dir.path().join("symlink");
        std::os::unix::fs::symlink(&ordinary_file, &symlink).unwrap();
        assert!(!start_at(unsafe { &*handle }, &symlink));
        assert!(
            std::fs::symlink_metadata(&symlink)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!owned_path.exists());
        // SAFETY: stop joins the worker before its callback context is freed.
        unsafe {
            droidloom_input_destroy(handle);
            assert_eq!((*context).starts.load(Ordering::SeqCst), 4);
            assert_eq!((*context).stops.load(Ordering::SeqCst), 4);
            let failed = droidloom_input_create(context as usize, inject, startup_failure);
            assert!(!droidloom_input_listen(
                failed,
                listener.as_fd().as_raw_fd()
            ));
            droidloom_input_destroy(failed);
            drop(Box::from_raw(context));
        }
    }
}
