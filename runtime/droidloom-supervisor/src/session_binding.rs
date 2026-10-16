//! Session-scoped environment records and non-inherited lifecycle locks.
//!
//! The CLI owns the record. Its consumer receives the same values from systemd;
//! no shell evaluation or user-manager-wide environment mutation is required.

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::{MaybeUninit, size_of};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};

use droidloom_window_policy::SessionMode;

const MAX_RECORD_BYTES: u64 = 8192;
const MAX_VALUE_BYTES: usize = 4096;
const KEYS: [&str; 8] = [
    "DROIDLOOM_MODE",
    "DROIDLOOM_HOST_NAVIGATION",
    "WAYLAND_DISPLAY",
    "DROIDLOOM_COMPOSITOR_PID",
    "DROIDLOOM_COMPOSITOR_START_TICKS",
    "XDG_SESSION_ID",
    "DISPLAY",
    "XDG_CURRENT_DESKTOP",
];

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Identity of the compositor that owns a local Wayland socket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompositorIdentity {
    /// Canonical absolute socket path.
    pub socket: PathBuf,
    /// Linux peer PID at connection time.
    pub pid: u32,
    /// `/proc/PID/stat` start time, preventing PID-reuse matches.
    pub start_ticks: u64,
}

/// Complete environment for one presenter session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionBinding {
    /// Explicit presentation preference; never an unresolved auto mode.
    pub mode: SessionMode,
    /// Qualified host chrome assertion, not authentication.
    pub host_navigation: bool,
    /// Bound compositor identity.
    pub compositor: CompositorIdentity,
    /// Optional logind label; not used as an authorization token.
    pub session_id: Option<String>,
    /// Optional Xwayland environment for this session.
    pub display: Option<String>,
    /// Desktop label used by ordinary desktop application matching.
    pub current_desktop: Option<String>,
}

/// Older clients wrote only the mode; that cannot prove ownership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionRecord {
    /// A mode-only record from a legacy client.
    Legacy(SessionMode),
    /// A fully bound current record.
    Bound(SessionBinding),
}

impl SessionRecord {
    /// Recover the explicit mode without interpreting an old record as a lease.
    pub fn mode(&self) -> SessionMode {
        match self {
            Self::Legacy(mode) => *mode,
            Self::Bound(binding) => binding.mode,
        }
    }
}

impl SessionBinding {
    /// Probe the caller's local Wayland connection without issuing protocol requests.
    pub fn from_environment(runtime: &Path, mode: SessionMode) -> io::Result<Self> {
        let name = env::var("WAYLAND_DISPLAY")
            .map_err(|_| invalid("WAYLAND_DISPLAY is required to start a graphical session"))?;
        let uid = fs::metadata("/proc/self")?.uid();
        let compositor = probe_compositor(runtime, &name, uid)?;
        let host_navigation = match env::var("DROIDLOOM_HOST_NAVIGATION").as_deref() {
            Ok("1") => true,
            Ok("0") | Err(_) => false,
            Ok(_) => {
                eprintln!("Droidloom ignored invalid DROIDLOOM_HOST_NAVIGATION; client controls remain enabled");
                false
            }
        };
        Ok(Self {
            mode,
            host_navigation,
            compositor,
            session_id: optional_environment("XDG_SESSION_ID")?,
            display: optional_environment("DISPLAY")?,
            current_desktop: optional_environment("XDG_CURRENT_DESKTOP")?,
        })
    }

    /// Whether an existing session belongs to this exact compositor instance.
    pub fn same_owner(&self, other: &Self) -> bool {
        self.compositor == other.compositor
    }

    /// Encode only allowlisted literal assignments understood by EnvironmentFile.
    pub fn encode(&self) -> io::Result<String> {
        let socket = self.compositor.socket.to_str().ok_or_else(|| invalid("Wayland socket is not UTF-8"))?;
        validate_socket_text(socket)?;
        if self.compositor.pid == 0 || self.compositor.start_ticks == 0 {
            return Err(invalid("compositor identity must be nonzero"));
        }
        let values = [
            self.mode.to_string(),
            u8::from(self.host_navigation).to_string(),
            socket.to_owned(),
            self.compositor.pid.to_string(),
            self.compositor.start_ticks.to_string(),
            self.session_id.clone().unwrap_or_default(),
            self.display.clone().unwrap_or_default(),
            self.current_desktop.clone().unwrap_or_default(),
        ];
        let mut result = String::new();
        for (key, value) in KEYS.iter().zip(values) {
            validate_value(&value)?;
            result.push_str(key);
            result.push_str("=\"");
            for character in value.chars() {
                if matches!(character, '\\' | '"') {
                    result.push('\\');
                }
                result.push(character);
            }
            result.push_str("\"\n");
        }
        if result.len() as u64 > MAX_RECORD_BYTES {
            return Err(invalid("session environment exceeds 8 KiB"));
        }
        Ok(result)
    }
}

fn optional_environment(key: &str) -> io::Result<Option<String>> {
    match env::var(key) {
        Ok(value) => {
            validate_value(&value)?;
            Ok((!value.is_empty()).then_some(value))
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(invalid(format!("{key} is not UTF-8"))),
    }
}

fn validate_value(value: &str) -> io::Result<()> {
    if value.len() > MAX_VALUE_BYTES || value.chars().any(char::is_control) {
        return Err(invalid("invalid or oversized session environment value"));
    }
    Ok(())
}

fn decode_value(encoded: &str) -> io::Result<String> {
    let value = if let Some(body) = encoded.strip_prefix('"') {
        let body = body.strip_suffix('"').ok_or_else(|| invalid("unclosed session value"))?;
        let mut result = String::new();
        let mut chars = body.chars();
        while let Some(character) = chars.next() {
            match character {
                '\\' => match chars.next() {
                    Some(next @ ('\\' | '"')) => result.push(next),
                    _ => return Err(invalid("unsupported session value escape")),
                },
                '"' => return Err(invalid("unescaped quote in session value")),
                other => result.push(other),
            }
        }
        result
    } else {
        // Legacy mode records were unquoted. Keep a deliberately narrow grammar.
        if encoded.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\')) {
            return Err(invalid("session values with special characters must be quoted"));
        }
        encoded.to_owned()
    };
    validate_value(&value)?;
    Ok(value)
}

/// Parse a bounded, literal session record. Unknown and duplicate keys are rejected.
pub fn parse_record(text: &str) -> io::Result<SessionRecord> {
    if text.len() as u64 > MAX_RECORD_BYTES || text.contains('\0') {
        return Err(invalid("invalid or oversized session environment"));
    }
    let mut values = BTreeMap::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| invalid("invalid session assignment"))?;
        if !KEYS.contains(&key) || values.insert(key, decode_value(value)?).is_some() {
            return Err(invalid("unknown or duplicate session environment key"));
        }
    }
    let mode = values.get("DROIDLOOM_MODE")
        .ok_or_else(|| invalid("session mode is missing"))?
        .parse::<SessionMode>().map_err(|_| invalid("mode must be desktop or mobile"))?;
    if values.len() == 1 {
        return Ok(SessionRecord::Legacy(mode));
    }
    let required = |key| values.get(key).filter(|v| !v.is_empty())
        .ok_or_else(|| invalid(format!("session binding lacks {key}")));
    let socket = required("WAYLAND_DISPLAY")?;
    validate_socket_text(socket)?;
    let pid = required("DROIDLOOM_COMPOSITOR_PID")?.parse::<u32>()
        .map_err(|_| invalid("invalid compositor PID"))?;
    let start_ticks = required("DROIDLOOM_COMPOSITOR_START_TICKS")?.parse::<u64>()
        .map_err(|_| invalid("invalid compositor start time"))?;
    if pid == 0 || start_ticks == 0 {
        return Err(invalid("compositor identity must be nonzero"));
    }
    let host_navigation = match values.get("DROIDLOOM_HOST_NAVIGATION").map(String::as_str) {
        Some("1") => true,
        Some("0") | None => false,
        _ => return Err(invalid("invalid recorded host-navigation assertion")),
    };
    let optional = |key| values.get(key).filter(|value| !value.is_empty()).cloned();
    Ok(SessionRecord::Bound(SessionBinding {
        mode,
        host_navigation,
        compositor: CompositorIdentity { socket: PathBuf::from(socket), pid, start_ticks },
        session_id: optional("XDG_SESSION_ID"),
        display: optional("DISPLAY"),
        current_desktop: optional("XDG_CURRENT_DESKTOP"),
    }))
}

fn validate_socket_text(value: &str) -> io::Result<()> {
    validate_value(value)?;
    if !Path::new(value).is_absolute()
        || Path::new(value).components().any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("Wayland socket must be an absolute normalized path"));
    }
    Ok(())
}

fn probe_compositor(runtime: &Path, name: &str, uid: u32) -> io::Result<CompositorIdentity> {
    validate_value(name)?;
    if name.is_empty() || Path::new(name).components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err(invalid("invalid WAYLAND_DISPLAY"));
    }
    let socket = if Path::new(name).is_absolute() { PathBuf::from(name) } else { runtime.join(name) };
    let metadata = fs::symlink_metadata(&socket)?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err(invalid("Wayland socket must be session-owned and must not be a symlink"));
    }
    let socket = socket.canonicalize()?;
    let stream = UnixStream::connect(&socket)?;
    let mut credentials = MaybeUninit::<libc::ucred>::uninit();
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the live stream and correctly sized output storage remain valid.
    let result = unsafe {
        libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(), &mut length)
    };
    if result != 0 { return Err(io::Error::last_os_error()); }
    if length as usize != size_of::<libc::ucred>() { return Err(invalid("invalid compositor credentials")); }
    // SAFETY: getsockopt successfully initialized a complete ucred above.
    let credentials = unsafe { credentials.assume_init() };
    if credentials.uid != uid { return Err(invalid("compositor belongs to another user")); }
    let pid = u32::try_from(credentials.pid).ok().filter(|pid| *pid != 0)
        .ok_or_else(|| invalid("invalid compositor PID"))?;
    Ok(CompositorIdentity { socket, pid, start_ticks: process_start_ticks(pid)? })
}

/// Read a process generation without relying on its name or a reused PID alone.
pub fn process_start_ticks(pid: u32) -> io::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat.rsplit_once(") ").ok_or_else(|| invalid("invalid process stat"))?;
    fields.split_whitespace().nth(19).ok_or_else(|| invalid("process start time is missing"))?
        .parse::<u64>().map_err(|_| invalid("invalid process start time"))
}

/// An exclusive lifecycle lock that does not survive exec in child processes.
#[derive(Debug)]
pub struct SessionLock { _file: File }

impl SessionLock {
    fn open(path: &Path, uid: u32, create: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(create).truncate(false)
            .mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o077 != 0 || meta.nlink() != 1 {
            return Err(invalid("lock is not a private session-owned regular file"));
        }
        // SAFETY: the owned descriptor remains live for this guard's lifetime.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }
}

/// The one private runtime directory belonging to the current desktop user.
pub struct SessionDirectory {
    runtime: PathBuf,
    directory: PathBuf,
    uid: u32,
}

impl SessionDirectory {
    /// Resolve and validate the directory before any service or environment mutation.
    pub fn from_environment() -> io::Result<Self> {
        let runtime = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
            .ok_or_else(|| invalid("XDG_RUNTIME_DIR is required for session lifecycle operations"))?;
        Self::open(runtime)
    }

    fn open(runtime: PathBuf) -> io::Result<Self> {
        if !runtime.is_absolute() || runtime.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
            return Err(invalid("XDG_RUNTIME_DIR must be absolute and normalized"));
        }
        let uid = fs::metadata("/proc/self")?.uid();
        let root = fs::symlink_metadata(&runtime)?;
        if !root.is_dir() || root.uid() != uid || root.mode() & 0o022 != 0 {
            return Err(invalid("runtime root must be a session-owned directory"));
        }
        let directory = runtime.join("droidloom");
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
            return Err(invalid("Droidloom runtime must be a session-owned directory"));
        }
        Ok(Self { runtime, directory, uid })
    }

    /// Runtime root used to resolve the caller's Wayland socket.
    pub fn runtime(&self) -> &Path { &self.runtime }

    /// Serialize user-facing lifecycle transactions; the presenter never takes this lock.
    pub fn lock_commands(&self) -> io::Result<SessionLock> {
        SessionLock::open(&self.directory.join("command.lock"), self.uid, true)
    }

    /// Determine whether a presenter still owns its lease without creating a new lease file.
    pub fn owner_locked(&self) -> io::Result<bool> {
        match SessionLock::open(&self.directory.join("owner.lock"), self.uid, false) {
            Ok(_guard) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error),
        }
    }

    /// Read the existing bounded record without following links.
    pub fn read_record(&self) -> io::Result<Option<SessionRecord>> {
        let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.directory.join("session.env")) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != self.uid || meta.mode() & 0o022 != 0 || meta.len() > MAX_RECORD_BYTES {
            return Err(invalid("invalid session environment file"));
        }
        let mut text = String::new();
        file.take(MAX_RECORD_BYTES + 1).read_to_string(&mut text)?;
        parse_record(&text).map(Some)
    }

    /// Atomically publish a complete binding only after the caller has checked ownership.
    pub fn write_binding(&self, binding: &SessionBinding) -> io::Result<()> {
        let encoded = binding.encode()?;
        fs::set_permissions(&self.directory, fs::Permissions::from_mode(0o700))?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        temporary.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.write_all(encoded.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(self.directory.join("session.env")).map_err(|error| error.error)?;
        File::open(&self.directory)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    fn binding() -> SessionBinding {
        SessionBinding {
            mode: SessionMode::Mobile,
            host_navigation: true,
            compositor: CompositorIdentity { socket: PathBuf::from("/run/user/1000/wayland-0"), pid: 123, start_ticks: 456 },
            session_id: Some("seat session".into()),
            display: Some(":0".into()),
            current_desktop: Some("Denial \"quoted\" \\ literal $value".into()),
        }
    }

    #[test]
    fn record_round_trip_is_literal_and_clears_missing_optional_values() {
        let value = binding();
        assert_eq!(parse_record(&value.encode().unwrap()).unwrap(), SessionRecord::Bound(value));
        let mut value = binding();
        value.display = None;
        assert!(value.encode().unwrap().contains("DISPLAY=\"\"\n"));
        assert_eq!(parse_record(&value.encode().unwrap()).unwrap(), SessionRecord::Bound(value));
    }

    #[test]
    fn legacy_mode_is_not_an_ownership_record() {
        assert_eq!(parse_record("DROIDLOOM_MODE=mobile\n").unwrap(), SessionRecord::Legacy(SessionMode::Mobile));
        assert!(parse_record("DROIDLOOM_MODE=auto\n").is_err());
        assert!(parse_record("DROIDLOOM_MODE=mobile\nWAYLAND_DISPLAY=/run/wayland\n").is_err());
    }

    #[test]
    fn malformed_or_ambiguous_records_are_rejected() {
        for text in [
            "DROIDLOOM_MODE=desktop\nDROIDLOOM_MODE=mobile\n",
            "DROIDLOOM_MODE=desktop\nPATH=/evil\n",
            "DROIDLOOM_MODE=\"desktop\" trailing\n",
            "DROIDLOOM_MODE=\"desktop\\n\"\n",
            "DROIDLOOM_MODE=desktop\0\n",
        ] { assert!(parse_record(text).is_err(), "{text:?}"); }
        assert!(parse_record(&"a".repeat(8193)).is_err());
        let mut value = binding();
        value.current_desktop = Some("Denial\nPATH=/evil".into());
        assert!(value.encode().is_err());
        value = binding();
        value.compositor.socket = PathBuf::from("/run/user/1000/../other/socket");
        assert!(value.encode().is_err());
    }

    #[test]
    fn compositor_probe_binds_the_actual_peer_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("wayland-test");
        let _listener = UnixListener::bind(&socket).unwrap();
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let identity = probe_compositor(temporary.path(), "wayland-test", uid).unwrap();
        assert_eq!(identity.pid, std::process::id());
        assert_eq!(identity.start_ticks, process_start_ticks(std::process::id()).unwrap());
        assert_eq!(identity.socket, socket);
        assert!(probe_compositor(temporary.path(), "../wayland-test", uid).is_err());
    }

    #[test]
    fn runtime_locks_are_exclusive_and_close_on_exec() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = SessionDirectory::open(temporary.path().to_owned()).unwrap();
        let guard = directory.lock_commands().unwrap();
        assert!(matches!(directory.lock_commands(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
        let mut child = Command::new("sh").args(["-c", "printf ready; sleep 0.3"])
            .stdout(Stdio::piped()).spawn().unwrap();
        let mut ready = [0; 5];
        child.stdout.as_mut().unwrap().read_exact(&mut ready).unwrap();
        drop(guard);
        assert!(directory.lock_commands().is_ok(), "exec child must not retain the lifecycle lock");
        child.wait().unwrap();
    }

    #[test]
    fn record_write_is_private_and_owner_probe_does_not_create_a_lease() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = SessionDirectory::open(temporary.path().to_owned()).unwrap();
        assert!(!directory.owner_locked().unwrap());
        assert!(!directory.directory.join("owner.lock").exists());
        directory.write_binding(&binding()).unwrap();
        assert_eq!(directory.read_record().unwrap(), Some(SessionRecord::Bound(binding())));
        assert_eq!(fs::metadata(directory.directory.join("session.env")).unwrap().mode() & 0o777, 0o600);
        let lease = SessionLock::open(&directory.directory.join("owner.lock"), directory.uid, true).unwrap();
        assert!(directory.owner_locked().unwrap());
        drop(lease);
        assert!(!directory.owner_locked().unwrap());
    }

    #[test]
    fn symlinks_are_not_runtime_directories_or_records_or_locks() {
        let temporary = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(target.path(), temporary.path().join("droidloom")).unwrap();
        assert!(SessionDirectory::open(temporary.path().to_owned()).is_err());
        fs::remove_file(temporary.path().join("droidloom")).unwrap();
        let directory = SessionDirectory::open(temporary.path().to_owned()).unwrap();
        let outside = temporary.path().join("outside");
        fs::write(&outside, "DROIDLOOM_MODE=desktop\n").unwrap();
        std::os::unix::fs::symlink(&outside, directory.directory.join("session.env")).unwrap();
        assert!(directory.read_record().is_err());
        std::os::unix::fs::symlink(&outside, directory.directory.join("command.lock")).unwrap();
        assert!(directory.lock_commands().is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "DROIDLOOM_MODE=desktop\n");
    }
}
