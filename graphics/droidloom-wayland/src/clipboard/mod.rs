//! Clipboard synchronization for ordinary Wayland compositors and Android.
mod backend;
mod wire;
use backend::{Backend, Offer, Source};
const BUILD_COMPATIBILITY: &str = "DROIDLOOM_CLIPBOARD_ABI=1;";
use crate::App;
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, OwnedFd, RawFd},
        unix::{
            fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
            net::{UnixDatagram, UnixListener, UnixStream},
        },
    },
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};
use wayland_client::{QueueHandle, globals::GlobalList, protocol::wl_seat};
use wire::{Blob, Clip, Description, MAX_BYTES, MAX_FILES, MAX_TEXT, Message, Received, Receiver};

const MARKER: &str = "application/x-droidloom-clipboard-";
const TEXT: &str = "text/plain;charset=utf-8";
const URI: &str = "text/uri-list";
const GNOME: &str = "x-special/gnome-copied-files";
#[derive(Clone)]
enum Outbound {
    Clip(Arc<Clip>),
    Sync(u64),
}
struct Mailbox {
    value: Mutex<(Option<Outbound>, bool)>,
    ready: Condvar,
}
impl Mailbox {
    fn new() -> Self {
        Self {
            value: Mutex::new((None, false)),
            ready: Condvar::new(),
        }
    }
    fn set(&self, v: Outbound) {
        self.value.lock().unwrap().0 = Some(v);
        self.ready.notify_one();
    }
    fn close(&self) {
        self.value.lock().unwrap().1 = true;
        self.ready.notify_all();
    }
}
enum Event {
    Native(u64, io::Result<Clip>),
    Android(u64, Received),
    Disconnected(u64),
}
#[derive(Clone)]
struct Events {
    tx: SyncSender<Event>,
    wake: Arc<UnixDatagram>,
}
impl Events {
    fn send(&self, e: Event) {
        if self.tx.send(e).is_ok() {
            let _ = self.wake.send(&[1]);
        }
    }
}
struct Peer {
    socket: UnixStream,
    mailbox: Arc<Mailbox>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.mailbox.close();
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

pub(super) struct Clipboard {
    backend: Backend,
    ready: bool,
    listener: UnixListener,
    socket_path: PathBuf,
    directory: Arc<tempfile::TempDir>,
    wake: UnixDatagram,
    events: Events,
    rx: mpsc::Receiver<Event>,
    peer: Option<Peer>,
    connection: u64,
    revision: u64,
    session: String,
    generation: Arc<AtomicU64>,
    workers: Arc<AtomicUsize>,
    offer: Option<Offer>,
    source: Option<Source>,
    current: Option<Arc<Clip>>,
    publish_pending: bool,
    focused: bool,
    serial: Option<u32>,
    awaiting: Option<String>,
    pending_since: Option<Instant>,
}
impl Clipboard {
    #[cfg(test)]
    pub fn test_text(&self) -> Option<&str> {
        self.current.as_ref().and_then(|c|c.description.text.as_deref())
    }
    #[cfg(test)]
    pub fn test_publish(&mut self, text: &str) {
        self.current=Some(Arc::new(Clip { description: Description {
            id: "test-export".into(), text: Some(text.into()), ..Default::default()
        }, files: vec![] }));
        self.publish_pending=true; self.focused=true; self.serial=Some(42); self.ready=true;
    }

    pub fn new(path: PathBuf, globals: &GlobalList, qh: &QueueHandle<App>) -> io::Result<Self> {
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_socket() => fs::remove_file(&path)?,
            Ok(_) => return Err(wire::invalid()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        eprintln!("{BUILD_COMPATIBILITY}");
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let directory = Arc::new(
            tempfile::Builder::new()
                .prefix("clipboard-")
                .tempdir_in(path.parent().unwrap())?,
        );
        let (wake, notify) = UnixDatagram::pair()?;
        wake.set_nonblocking(true)?;
        notify.set_nonblocking(true)?;
        let (tx, rx) = mpsc::sync_channel(32);
        let session = fs::read_to_string("/proc/sys/kernel/random/uuid")?
            .trim()
            .replace('-', "");
        Ok(Self {
            backend: Backend::new(globals, qh),
            ready: true,
            listener,
            socket_path: path,
            directory,
            wake,
            events: Events {
                tx,
                wake: Arc::new(notify),
            },
            rx,
            peer: None,
            connection: 0,
            revision: 0,
            session,
            generation: Arc::new(AtomicU64::new(0)),
            workers: Arc::new(AtomicUsize::new(0)),
            offer: None,
            source: None,
            current: None,
            publish_pending: false,
            focused: false,
            serial: None,
            awaiting: None,
            pending_since: None,
        })
    }
    pub fn seat(&mut self, seat: &wl_seat::WlSeat, qh: &QueueHandle<App>) {
        self.backend.seat(seat, qh);
    }
    pub fn focus(&mut self, focused: bool) {
        self.ready = true;
        self.focused = focused;
        if !focused {
            self.serial = None;
        }
    }
    pub fn serial(&mut self, serial: u32) {
        self.ready = true;
        self.serial = Some(serial);
    }
    pub fn blocked(&self) -> bool {
        self.pending_since
            .is_some_and(|s| s.elapsed() < Duration::from_secs(10))
            && self.peer.is_some()
    }
    pub fn unblock_deadline(&self) -> Option<Instant> {
        self.peer.as_ref()?;
        self.pending_since.map(|since| since + Duration::from_secs(10))
    }
    pub fn fds(&self) -> [(RawFd, i16); 2] {
        [
            (self.listener.as_raw_fd(), libc::POLLIN),
            (self.wake.as_raw_fd(), libc::POLLIN),
        ]
    }
    fn id(&self) -> String {
        format!("{}-{}", self.session, self.revision)
    }
    fn outbound(&self, command: Outbound) {
        if let Some(p) = &self.peer {
            p.mailbox.set(command);
        }
    }
    fn import(&mut self, clip: Clip) {
        let clip = Arc::new(clip);
        self.awaiting = Some(clip.description.id.clone());
        self.pending_since = Some(Instant::now());
        self.current = Some(clip.clone());
        self.outbound(Outbound::Clip(clip));
    }
    pub fn selection(&mut self, offer: Option<Offer>) {
        self.ready = true;
        if let Some(old) = self.offer.take() {
            old.destroy();
        }
        let mimes = offer.as_ref().map(Offer::mimes).unwrap_or_default();
        if self
            .current
            .as_ref()
            .is_some_and(|c| mimes.contains(&format!("{MARKER}{}", c.description.id)))
        {
            self.offer = offer;
            return;
        }
        // A compositor echo of our explicit clear must not generate a new clear.
        if offer.is_none() && self.current.as_ref().is_some_and(|c| c.description.empty()) {
            return;
        }
        self.revision += 1;
        self.generation.store(self.revision, Ordering::Release);
        self.publish_pending = false;
        self.awaiting = None;
        self.pending_since = Some(Instant::now());
        self.outbound(Outbound::Sync(self.revision));
        let desc = Description {
            id: self.id(),
            base: self.revision,
            ..Default::default()
        };
        let Some(offer) = offer else {
            self.import(Clip {
                description: desc,
                files: vec![],
            });
            return;
        };
        let mut selected = Vec::new();
        if let Some(m) = [
            TEXT,
            "text/plain;charset=UTF-8",
            "UTF8_STRING",
            "text/plain",
        ]
        .into_iter()
        .find(|m| mimes.iter().any(|v| v == m))
        {
            selected.push(m.to_string());
        }
        if mimes.iter().any(|m| m == "text/html") {
            selected.push("text/html".into());
        }
        if mimes.iter().any(|m| m == URI) {
            selected.push(URI.into());
        } else if mimes.iter().any(|m| m == GNOME) {
            selected.push(GNOME.into());
        } else if let Some(m) = mimes
            .iter()
            .find(|m| m.as_str() == "image/png")
            .or_else(|| mimes.iter().find(|m| m.starts_with("image/")))
        {
            selected.push(m.clone());
        }
        if selected.is_empty() {
            self.offer = Some(offer);
            self.import(Clip {
                description: desc,
                files: vec![],
            });
            return;
        }
        if self.workers.fetch_add(1, Ordering::AcqRel) >= 8 {
            self.workers.fetch_sub(1, Ordering::AcqRel);
            self.offer = Some(offer);
            self.pending_since = None;
            return;
        }
        let mut pipes = Vec::new();
        for mime in selected {
            match pipe() {
                Ok((read, write)) => {
                    offer.receive(&mime, write.as_fd());
                    pipes.push((mime, read));
                }
                Err(_) => {
                    self.workers.fetch_sub(1, Ordering::AcqRel);
                    self.pending_since = None;
                    self.offer = Some(offer);
                    return;
                }
            }
        }
        self.offer = Some(offer);
        let events = self.events.clone();
        let directory = self.directory.clone();
        let generation = self.generation.clone();
        let workers = self.workers.clone();
        let rev = self.revision;
        spawn_background("dl-clip-native", move || {
            let result = read_native(desc, pipes, &directory, &generation, rev);
            workers.fetch_sub(1, Ordering::AcqRel);
            events.send(Event::Native(rev, result));
        });
    }
    pub fn notify_ready(&mut self, descriptors: &[libc::pollfd]) {
        let fds = self.fds();
        if descriptors
            .iter()
            .any(|p| p.revents != 0 && fds.iter().any(|(fd, _)| *fd == p.fd))
        {
            self.ready = true;
        }
    }
    pub fn pump(&mut self, qh: &QueueHandle<App>) {
        if !self.ready {
            return;
        }
        self.ready = false;
        while let Ok((socket, _)) = self.listener.accept() {
            if !root_peer(&socket).unwrap_or(false) {
                continue;
            }
            self.peer = None;
            self.connection += 1;
            let connection = self.connection;
            let mailbox = Arc::new(Mailbox::new());
            let Ok(mut input) = socket.try_clone() else {
                continue;
            };
            let Ok(mut output) = socket.try_clone() else {
                continue;
            };
            let events = self.events.clone();
            let dir = self.directory.clone();
            let close = socket.try_clone().unwrap();
            spawn_background("dl-clip-read", move || {
                let mut reader = Receiver::new(dir.path().into());
                let mut hello = false;
                loop {
                    match reader.read(&mut input) {
                        Ok(Some(Received::Message(Message::Hello { abi: wire::ABI })))
                            if !hello =>
                        {
                            hello = true
                        }
                        Ok(Some(event)) if hello => events.send(Event::Android(connection, event)),
                        Ok(None) if hello => {}
                        _ => break,
                    }
                }
                let _ = close.shutdown(std::net::Shutdown::Both);
                events.send(Event::Disconnected(connection));
            });
            let pending = mailbox.clone();
            let close = socket.try_clone().unwrap();
            spawn_background("dl-clip-write", move || {
                let mut result =
                    wire::send_message(&mut output, &Message::Hello { abi: wire::ABI });
                while result.is_ok() {
                    let mut state = pending.value.lock().unwrap();
                    while state.0.is_none() && !state.1 {
                        state = pending.ready.wait(state).unwrap();
                    }
                    if state.1 {
                        break;
                    }
                    let next = state.0.take().unwrap();
                    drop(state);
                    result = match next {
                        Outbound::Clip(c) => wire::send_clip(&mut output, &c),
                        Outbound::Sync(revision) => {
                            wire::send_message(&mut output, &Message::Sync { revision })
                        }
                    };
                }
                let _ = close.shutdown(std::net::Shutdown::Both);
            });
            self.peer = Some(Peer { socket, mailbox });
            if let Some(c) = &self.current {
                self.outbound(Outbound::Clip(c.clone()));
            } else {
                self.outbound(Outbound::Sync(self.revision));
            }
            eprintln!("Droidloom clipboard: Android connected");
        }
        let mut bytes = [0; 128];
        while self.wake.recv(&mut bytes).is_ok() {}
        for _ in 0..64 {
            let Ok(event) = self.rx.try_recv() else {
                break;
            };
            match event {
                Event::Native(rev, Ok(clip)) if rev == self.revision => self.import(clip),
                Event::Native(rev, Err(_)) if rev == self.revision => {
                    self.pending_since = None;
                    eprintln!("Droidloom clipboard: source transfer failed or exceeded limits");
                }
                Event::Android(connection, Received::Clip(mut clip))
                    if connection == self.connection =>
                {
                    if clip.description.base != self.revision {
                        self.outbound(Outbound::Sync(self.revision));
                        continue;
                    }
                    self.revision += 1;
                    self.generation.store(self.revision, Ordering::Release);
                    clip.description.base = self.revision;
                    self.awaiting = None;
                    self.pending_since = None;
                    self.current = Some(Arc::new(clip));
                    self.publish_pending = true;
                    self.outbound(Outbound::Sync(self.revision));
                }
                Event::Android(connection, Received::Message(Message::Ack { id }))
                    if connection == self.connection =>
                {
                    if self.awaiting.as_ref() == Some(&id) {
                        self.awaiting = None;
                        self.pending_since = None;
                    }
                }
                Event::Disconnected(connection) if connection == self.connection => {
                    self.peer = None;
                    self.awaiting = None;
                    self.pending_since = None;
                    eprintln!("Droidloom clipboard: Android disconnected");
                }
                _ => {}
            }
        }
        if self.publish_pending
            && (self.backend.background() || (self.focused && self.serial.is_some()))
        {
            if let Some(c) = &self.current {
                let new = self.backend.publish(c.clone(), self.serial, qh);
                if let Some(old) = self.source.replace_opt(new) {
                    old.destroy();
                }
            }
            self.publish_pending = false;
        }
    }
    pub fn send_offer(&self, clip: Arc<Clip>, mime: String, fd: OwnedFd) {
        if self.workers.fetch_add(1, Ordering::AcqRel) >= 8 {
            self.workers.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        let workers = self.workers.clone();
        spawn_background("dl-clip-offer", move || {
            let _ = write_offer(&clip, &mime, fd);
            workers.fetch_sub(1, Ordering::AcqRel);
        });
    }
}
// Option::replace with an optional incoming value.
trait ReplaceOption<T> {
    fn replace_opt(&mut self, value: Option<T>) -> Option<T>;
}
impl<T> ReplaceOption<T> for Option<T> {
    fn replace_opt(&mut self, value: Option<T>) -> Option<T> {
        std::mem::replace(self, value)
    }
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        self.generation.store(u64::MAX, Ordering::Release);
        self.peer = None;
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn pipe() -> io::Result<(File, File)> {
    let mut fds = [-1; 2];
    // SAFETY: storage holds exactly the two output descriptors; both are owned below.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    use std::os::fd::FromRawFd;
    // SAFETY: successful pipe2 produced two fresh descriptors.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}
fn root_peer(s: &UnixStream) -> io::Result<bool> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: !0,
        gid: !0,
    };
    let mut size = std::mem::size_of_val(&cred) as libc::socklen_t;
    // SAFETY: valid socket, writable credential storage, and matching length.
    if unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid == 0)
}
fn read_native(
    mut desc: Description,
    pipes: Vec<(String, File)>,
    directory: &tempfile::TempDir,
    generation: &AtomicU64,
    rev: u64,
) -> io::Result<Clip> {
    let mut files = vec![];
    let mut total = 0;
    let deadline = Instant::now() + Duration::from_secs(30);
    for (mime, mut input) in pipes {
        let mut temp = tempfile::NamedTempFile::new_in(directory.path())?;
        let mut buffer = [0; 65536];
        let mut size = 0;
        loop {
            if generation.load(Ordering::Acquire) != rev || Instant::now() > deadline {
                return Err(wire::invalid());
            }
            let mut p = libc::pollfd {
                fd: input.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd, no retained pointer.
            let ready = unsafe { libc::poll(&raw mut p, 1, 100) };
            if ready < 0 {
                return Err(io::Error::last_os_error());
            }
            if ready == 0 {
                continue;
            }
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            if size
                > if mime.starts_with("image/") {
                    MAX_BYTES
                } else {
                    MAX_TEXT as u64
                }
            {
                return Err(wire::invalid());
            }
            temp.write_all(&buffer[..n])?;
        }
        if mime.starts_with("image/") {
            desc.blobs.push(Blob {
                mime: mime.clone(),
                name: format!("clipboard.{}", mime.strip_prefix("image/").unwrap_or("bin")),
                size,
            });
            files.push(Arc::new(temp));
            total += size;
        } else {
            let text = fs::read_to_string(temp.path())?;
            if mime == URI || mime == GNOME {
                let mut urls = vec![];
                for line in text
                    .lines()
                    .filter(|l| !l.is_empty() && !l.starts_with('#') && *l != "copy" && *l != "cut")
                {
                    if generation.load(Ordering::Acquire) != rev {
                        return Err(wire::invalid());
                    }
                    let url = url::Url::parse(line).map_err(|_| wire::invalid())?;
                    if url.scheme() != "file" {
                        urls.push(line);
                        continue;
                    }
                    let path = url.to_file_path().map_err(|_| wire::invalid())?;
                    let mut input = fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&path)?;
                    let meta = input.metadata()?;
                    if !meta.is_file()
                        || meta.len() > MAX_BYTES - total
                        || desc.blobs.len() >= MAX_FILES
                    {
                        return Err(wire::invalid());
                    }
                    let name =
                        wire::safe_name(&path.file_name().unwrap_or_default().to_string_lossy());
                    let mut f = tempfile::Builder::new()
                        .prefix("clip-")
                        .suffix(&format!("-{name}"))
                        .tempfile_in(directory.path())?;
                    let count = io::copy(&mut (&mut input).take(MAX_BYTES - total + 1), &mut f)?;
                    total += count;
                    if total > MAX_BYTES {
                        return Err(wire::invalid());
                    }
                    let mime = match path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "gif" => "image/gif",
                        "webp" => "image/webp",
                        "txt" => "text/plain",
                        "pdf" => "application/pdf",
                        _ => "application/octet-stream",
                    };
                    desc.blobs.push(Blob {
                        mime: mime.into(),
                        name,
                        size: count,
                    });
                    files.push(Arc::new(f));
                }
                if desc.text.is_none() && !urls.is_empty() {
                    desc.text = Some(urls.join("\n"));
                }
            } else if mime == "text/html" {
                desc.html = Some(text);
            } else {
                desc.text = Some(text);
            }
        }
    }
    desc.validate()?;
    Ok(Clip {
        description: desc,
        files,
    })
}
pub fn mimes(c: &Clip) -> Vec<String> {
    let mut v = vec![format!("{MARKER}{}", c.description.id)];
    if c.description.text.is_some() {
        v.extend([TEXT.into(), "text/plain".into(), "UTF8_STRING".into()]);
    }
    if c.description.html.is_some() {
        v.push("text/html".into());
    }
    if !c.files.is_empty() {
        v.push(URI.into());
        v.push(GNOME.into());
        if c.files.len() == 1 && c.description.blobs[0].mime.starts_with("image/") {
            v.push(c.description.blobs[0].mime.clone());
        }
    }
    v
}
fn write_offer(c: &Clip, mime: &str, fd: OwnedFd) -> io::Result<()> {
    let mut out = File::from(fd);
    let flags = unsafe { libc::fcntl(out.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(out.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut input: Box<dyn Read> = if mime == "text/html" {
        Box::new(io::Cursor::new(
            c.description.html.clone().unwrap_or_default().into_bytes(),
        ))
    } else if mime == URI || mime == GNOME {
        let urls: Vec<_> = c
            .files
            .iter()
            .map(|f| url::Url::from_file_path(f.path()).unwrap().to_string())
            .collect();
        let text = if mime == GNOME {
            format!("copy\n{}\n", urls.join("\n"))
        } else {
            format!("{}\r\n", urls.join("\r\n"))
        };
        Box::new(io::Cursor::new(text.into_bytes()))
    } else if mime.starts_with(MARKER) {
        Box::new(io::Cursor::new(c.description.id.as_bytes().to_vec()))
    } else if mime.starts_with("image/")
        && c.files.len() == 1
        && c.description.blobs[0].mime == mime
    {
        Box::new(File::open(c.files[0].path())?)
    } else if [TEXT, "text/plain", "UTF8_STRING"].contains(&mime) {
        Box::new(io::Cursor::new(
            c.description.text.clone().unwrap_or_default().into_bytes(),
        ))
    } else {
        return Err(wire::invalid());
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut buf = [0; 65536];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let mut pending = &buf[..n];
        while !pending.is_empty() {
            if Instant::now() > deadline {
                return Err(wire::invalid());
            }
            match out.write(pending) {
                Ok(0) => return Err(wire::invalid()),
                Ok(n) => pending = &pending[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let mut p = libc::pollfd {
                        fd: out.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    unsafe {
                        libc::poll(&raw mut p, 1, 100);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(dir: &tempfile::TempDir, bytes: &[u8]) -> File {
        let mut f = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        f.write_all(bytes).unwrap();
        f.reopen().unwrap()
    }
    #[test]
    fn native_text_html_and_file_urls_preserve_content_and_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a file #é.txt");
        fs::write(&path, b"file contents\0\xff").unwrap();
        let list = format!("{}\r\n", url::Url::from_file_path(&path).unwrap());
        let pipes = vec![
            (TEXT.into(), input(&dir, "héllo 世界\n".as_bytes())),
            ("text/html".into(), input(&dir, b"<b>hello</b>")),
            (URI.into(), input(&dir, list.as_bytes())),
        ];
        let desc = Description {
            id: "test-1".into(),
            base: 1,
            ..Default::default()
        };
        let clip = read_native(desc, pipes, &dir, &AtomicU64::new(1), 1).unwrap();
        assert_eq!(clip.description.text.as_deref(), Some("héllo 世界\n"));
        assert_eq!(clip.description.html.as_deref(), Some("<b>hello</b>"));
        assert_eq!(clip.description.blobs[0].name, "a file #é.txt");
        assert_eq!(
            fs::read(clip.files[0].path()).unwrap(),
            b"file contents\0\xff"
        );
    }
    #[test]
    fn superseded_transfer_and_special_files_never_publish() {
        let dir = tempfile::tempdir().unwrap();
        let desc = Description {
            id: "test-1".into(),
            base: 1,
            ..Default::default()
        };
        assert!(
            read_native(
                desc.clone(),
                vec![(TEXT.into(), input(&dir, b"old"))],
                &dir,
                &AtomicU64::new(2),
                1
            )
            .is_err()
        );
        assert!(
            read_native(
                desc,
                vec![(URI.into(), input(&dir, b"file:///dev/zero\r\n"))],
                &dir,
                &AtomicU64::new(1),
                1
            )
            .is_err()
        );
    }
    #[test]
    fn replacing_queued_clipboard_work_keeps_only_latest() {
        let mailbox = Mailbox::new();
        for i in 0..1000 {
            mailbox.set(Outbound::Sync(i));
        }
        let state = mailbox.value.lock().unwrap();
        assert!(matches!(state.0, Some(Outbound::Sync(999))));
    }
}


fn spawn_background<F>(name: &str, work: F) -> std::thread::JoinHandle<()>
where F: FnOnce() + Send + 'static {
    droidloom_cpu_placement::spawn(name, droidloom_cpu_placement::Role::Background, work)
        .expect("spawn clipboard worker")
}
