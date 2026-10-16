//! Local lifecycle protocol shared by `droidloomd` and `droidloomctl`.
//!
//! The daemon owns the privileged Android cell. Its public socket may be
//! connectable by local users, but Linux `SO_PEERCRED` binds every mutating
//! request to root or to the exact host UID named by the cell specification.

#![deny(unsafe_op_in_unsafe_fn)]

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use droidloom_task_control::framework as framework_wire;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::CellSpec;
use crate::development::{
    DevelopmentCell, PenInputDevice, discover_pen_input_devices, recover_development_cell,
    refresh_pen_input_nodes, start_development_cell,
};

#[path = "diagnostics.rs"]
mod diagnostics;

#[path = "apk.rs"]
mod apk;

#[path = "framework_control.rs"]
mod framework_control;
pub use framework_control::subscribe_applications;
use framework_control::{framework_request, launch_in_cell, set_dpi_in_cell};

#[path = "control_progress.rs"]
mod progress;

#[path = "lifecycle_wait.rs"]
mod lifecycle_wait;
pub use lifecycle_wait::Shutdown;
use lifecycle_wait::WaitSources;

use progress::{LOG_HINT, Reporter};

/// Default root-owned lifecycle socket.
pub const DEFAULT_CONTROL_SOCKET: &str = "/run/droidloom/control.sock";
/// Default package-installed cell specification.
pub const DEFAULT_CELL_SPEC: &str = "/etc/droidloom/cell.json";
/// Root-owned directory from which non-root users may select specifications.
pub const DEFAULT_SPEC_DIRECTORY: &str = "/etc/droidloom";
/// Default small namespace-entry executable.
pub const DEFAULT_SUPERVISOR: &str = "/usr/bin/droidloom-supervisor";

const MAX_MESSAGE_BYTES: u64 = 64 * 1024;
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SPEC_BYTES: u64 = 1024 * 1024;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const ANDROID_READY_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_DISPLAY_DPI: u32 = 72;
const MIN_ICON_SIZE: u32 = 16;
const MAX_ICON_SIZE: u32 = 512;
const MAX_APPLICATIONS: usize = 1024;
const ANDROID_PER_USER_UID_RANGE: u32 = 100_000;
/// Paired direct framework launch/catalog transport.
pub const FRAMEWORK_CONTROL_ABI: &str = framework_wire::BUILD_ABI;

/// One control request. The externally tagged encoding is stable and easy to
/// inspect in traces without a bespoke binary parser.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlRequest {
    /// Delegate a direct, read-only framework catalog event subscription.
    WatchApplications,
    /// Render a bounded batch of launcher PNGs without starting ART processes.
    ApplicationIcons {
        /// Exact launcher components (at most 32).
        components: Vec<String>,
        /// Android user (currently zero).
        user: u32,
        /// Square PNG extent.
        size: u32,
    },
    /// Install a standalone APK supplied as a file descriptor on this connection.
    Install {
        /// Android user identifier.
        user: u32,
    },
    /// Start the cell if it is not already running.
    Start {
        /// Cell specification to validate and activate.
        spec: PathBuf,
    },
    /// Stop the current cell. Already-stopped is success.
    Stop,
    /// Stop and start the specified cell.
    Restart {
        /// Cell specification to validate and activate.
        spec: PathBuf,
    },
    /// Report daemon and cell state.
    Status,
    /// Wait for Android boot and Droidloom's input/task services to be ready.
    WaitReady,
    /// Read bounded Android logs or a crash report from the caller's cell.
    Diagnostics {
        /// Optional installed package; filters logs or selects exit history.
        package: Option<String>,
        /// Android user for package log UID lookup.
        user: u32,
        /// Maximum recent logcat entries (1 through 2000).
        lines: u32,
        /// Include the global crash buffer and recorded process exit reasons.
        crashes: bool,
    },
    /// Launch one Android package in the boot-managed cell.
    Launch {
        /// Expected boot-managed cell specification.
        spec: PathBuf,
        /// Android package name.
        package: String,
        /// Optional flattened `PACKAGE/ACTIVITY` component.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        component: Option<String>,
        /// Initial Android task extent as WIDTHxHEIGHT pixels; restarts this app.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolution: Option<String>,
        /// Android user identifier.
        user: u32,
    },
    /// List enabled Android MAIN/LAUNCHER activities.
    ListApplications {
        /// Android user identifier.
        user: u32,
    },
    /// Render one exact launcher's adaptive icon as PNG.
    ApplicationIcon {
        /// Flattened `PACKAGE/ACTIVITY` component.
        component: String,
        /// Android user identifier.
        user: u32,
        /// Square output extent in pixels.
        size: u32,
    },
    /// Persist the density of Android's single built-in display.
    SetDpi {
        /// Cell specification used when the runtime is cold.
        spec: PathBuf,
        /// Android density in dots per inch.
        dpi: u32,
    },
}

/// Coarse lifecycle state exposed to users and session integration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellState {
    /// No cell is owned by the daemon.
    Stopped,
    /// The namespace owner is alive. Android services may still be booting.
    Running,
}

/// One enabled Android launcher activity exposed to the host application menu.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidApplication {
    /// Localized Android label.
    pub name: String,
    /// Owning Android package.
    pub package: String,
    /// Exact flattened launcher component.
    pub component: String,
    /// Stable version/resource identity used to cache its rendered icon.
    pub icon_key: String,
}

/// One bounded control response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlResponse {
    /// Whether the requested operation completed successfully.
    pub ok: bool,
    /// Cell state after the operation.
    pub state: CellState,
    /// Human-readable result or error.
    pub message: String,
    /// Exact host PID owned by the daemon when running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_pid: Option<u32>,
    /// Successful task-launcher output, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<String>,
    /// Enabled MAIN/LAUNCHER activities, when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applications: Option<Vec<AndroidApplication>>,
    /// Base64-encoded PNG for one exact launcher component, when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_icon: Option<String>,
    /// Component-to-base64 PNG batch, when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_icons: Option<std::collections::BTreeMap<String, String>>,
    /// Android log snapshot or crash report, when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<String>,
}

/// Daemon filesystem and executable configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonConfig {
    /// Unix lifecycle socket.
    pub socket: PathBuf,
    /// Executable exposing `development-enter`.
    pub supervisor: PathBuf,
    /// Root-owned specification directory available to non-root clients.
    pub spec_directory: PathBuf,
}

/// Control transport or daemon failure.
#[derive(Debug, Error)]
pub enum ControlError {
    /// Filesystem, socket, or process operation failed.
    #[error("{context}: {source}")]
    Io {
        /// Operation being attempted.
        context: String,
        /// Underlying failure.
        source: io::Error,
    },
    /// JSON protocol data was malformed or incompatible.
    #[error("invalid lifecycle protocol message: {0}")]
    Protocol(#[from] serde_json::Error),
    /// A bounded local contract was violated.
    #[error("{0}")]
    Invalid(String),
}

struct ActiveCell {
    spec_path: PathBuf,
    spec: CellSpec,
    cell: DevelopmentCell,
    ready_init: std::cell::Cell<Option<u32>>,
    pen_devices: Vec<PenInputDevice>,
    pen_generation: u64,
    pen_refresh_pending: bool,
}
impl ActiveCell {
    fn ready_init(&self, progress: &Reporter) -> Result<u32, ControlError> {
        if let Some(pid) = self.ready_init.get() {
            return Ok(pid);
        }
        let pid = wait_for_android_init(&self.spec, progress)?;
        self.ready_init.set(Some(pid));
        Ok(pid)
    }
}

struct Daemon {
    config: DaemonConfig,
    active: Option<ActiveCell>,
    last_exit: Option<String>,
    progress: Reporter,
    pen_input_watcher: Option<PenInputWatcher>,
}

struct PenInputWatcher {
    fd: OwnedFd,
}

impl PenInputWatcher {
    fn new() -> io::Result<Self> {
        // SAFETY: inotify_init1 takes flags only and returns a fresh descriptor.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful inotify_init1 transferred its descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let path = std::ffi::CString::new("/dev/input").expect("static path has no NUL");
        let watch = unsafe {
            libc::inotify_add_watch(
                fd.as_raw_fd(),
                path.as_ptr(),
                libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO,
            )
        };
        if watch < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    fn drain(&self) -> io::Result<Vec<(u32, String)>> {
        let mut events = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            // SAFETY: read writes at most buffer.len() bytes into a valid buffer.
            let count = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::WouldBlock => break,
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(error),
                }
            }
            if count == 0 {
                break;
            }
            events.extend(parse_pen_input_events(
                &buffer[..usize::try_from(count).expect("nonnegative read length")],
            ));
        }
        Ok(events)
    }
}

fn parse_pen_input_events(bytes: &[u8]) -> Vec<(u32, String)> {
    let mut parsed = Vec::new();
    let mut offset = 0;
    while offset + 16 <= bytes.len() {
        let mask = u32::from_ne_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        let name_length = usize::try_from(u32::from_ne_bytes(
            bytes[offset + 12..offset + 16].try_into().unwrap(),
        ))
        .expect("inotify name length fits usize");
        let name_start = offset + 16;
        let Some(name_end) = name_start.checked_add(name_length) else {
            break;
        };
        if name_end > bytes.len() {
            break;
        }
        let name = bytes[name_start..name_end]
            .split(|byte| *byte == 0)
            .next()
            .unwrap_or_default();
        if mask & libc::IN_Q_OVERFLOW != 0 {
            parsed.push((mask, String::new()));
        } else if let Ok(name) = std::str::from_utf8(name)
            && is_host_event_name(name)
        {
            parsed.push((mask, name.to_owned()));
        }
        offset = name_end;
    }
    parsed
}

fn is_host_event_name(name: &str) -> bool {
    name.strip_prefix("event")
        .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()))
}

impl Daemon {
    fn new(config: DaemonConfig) -> Self {
        let pen_input_watcher = PenInputWatcher::new()
            .map_err(|error| {
                eprintln!("droidloomd: pen device hotplug watch unavailable: {error}");
                error
            })
            .ok();
        Self {
            config,
            active: None,
            last_exit: None,
            progress: Reporter::default(),
            pen_input_watcher,
        }
    }

    fn poll(&mut self) -> Result<(), ControlError> {
        if let Some(active) = self.active.as_mut() {
            match active.cell.try_wait().map_err(development_error) {
                Ok(Some(status)) => {
                    self.last_exit = Some(format!("Android cell exited with {status}"));
                    self.active = None;
                }
                Ok(None) => {}
                Err(error) => {
                    self.last_exit = Some(format!("Android cell teardown failed: {error}"));
                    self.active = None;
                    return Err(error);
                }
            }
        }

        let events = self
            .pen_input_watcher
            .as_ref()
            .map(PenInputWatcher::drain)
            .transpose()
            .map_err(|source| io_error("read host pen device events", source))?
            .unwrap_or_default();
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        let devices = discover_pen_input_devices().map_err(development_error)?;
        let previous_names = active
            .pen_devices
            .iter()
            .map(|device| device.event_name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let current_names = devices
            .iter()
            .map(|device| device.event_name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let hotplugged_pen = events.iter().any(|(mask, name)| {
            mask & libc::IN_Q_OVERFLOW != 0
                || previous_names.contains(name.as_str())
                || current_names.contains(name.as_str())
        });
        if hotplugged_pen || devices != active.pen_devices {
            active.pen_refresh_pending = true;
        }
        if active.pen_refresh_pending {
            let generation = active
                .pen_generation
                .checked_add(1)
                .ok_or_else(|| ControlError::Invalid("pen device generation exhausted".into()))?;
            active.pen_generation = generation;
            match refresh_pen_input_nodes(active.cell.host_pid(), generation, &devices) {
                Ok(()) => {
                    active.pen_devices = devices;
                    active.pen_refresh_pending = false;
                }
                Err(error) => {
                    eprintln!("droidloomd: could not refresh Android pen devices: {error}");
                }
            }
        }
        Ok(())
    }

    fn handle(
        &mut self,
        peer_uid: u32,
        request: ControlRequest,
        file: Option<fs::File>,
    ) -> ControlResponse {
        if let Err(error) = self.poll() {
            return self.failure(error.to_string());
        }
        let result = match request {
            ControlRequest::WatchApplications => Err(ControlError::Invalid(
                "subscription requires descriptor transport".into(),
            )),
            ControlRequest::ApplicationIcons {
                components,
                user,
                size,
            } => self.application_icons(peer_uid, &components, user, size),
            ControlRequest::Install { user } => self.install(peer_uid, user, file),
            ControlRequest::Start { spec } => self.start(peer_uid, &spec),
            ControlRequest::Stop => self.stop(peer_uid),
            ControlRequest::Restart { spec } => self.restart(peer_uid, &spec),
            ControlRequest::Status => self.status(peer_uid),
            ControlRequest::WaitReady => {
                self.require_active_cell(peer_uid, None).and_then(|active| {
                    // Explicit readiness requests recheck a possible system_server restart.
                    active.ready_init.set(None);
                    active.ready_init(&self.progress)?;
                    Ok(self.success("Droidloom is running; Android is ready"))
                })
            }
            ControlRequest::Diagnostics {
                package,
                user,
                lines,
                crashes,
            } => self.diagnostics(peer_uid, package.as_deref(), user, lines, crashes),
            ControlRequest::Launch {
                spec,
                package,
                component,
                resolution,
                user,
            } => self.launch(
                peer_uid,
                &spec,
                &package,
                component.as_deref(),
                resolution.as_deref(),
                user,
            ),
            ControlRequest::ListApplications { user } => self.list_applications(peer_uid, user),
            ControlRequest::ApplicationIcon {
                component,
                user,
                size,
            } => self.application_icon(peer_uid, &component, user, size),
            ControlRequest::SetDpi { spec, dpi } => self.set_dpi(peer_uid, &spec, dpi),
        };
        result.unwrap_or_else(|error| self.failure(error.to_string()))
    }

    fn start(&mut self, peer_uid: u32, spec_path: &Path) -> Result<ControlResponse, ControlError> {
        let spec = load_authorized_spec(peer_uid, spec_path, &self.config.spec_directory)?;
        if let Some(active) = &self.active {
            if active.spec == spec {
                return Ok(self.success(format!("cell {} is already running", spec.id())));
            }
            return Err(ControlError::Invalid(format!(
                "cell {} is already running from {}",
                active.spec.id(),
                active.spec_path.display()
            )));
        }

        recover_development_cell(&spec).map_err(development_error)?;
        let cell = start_development_cell(spec_path, &spec, &self.config.supervisor)
            .map_err(development_error)?;
        let pid = cell.host_pid();
        let pen_devices = discover_pen_input_devices().map_err(development_error)?;
        self.active = Some(ActiveCell {
            spec_path: spec_path.to_owned(),
            spec: spec.clone(),
            cell,
            ready_init: std::cell::Cell::new(None),
            pen_devices,
            pen_generation: 0,
            pen_refresh_pending: false,
        });
        self.last_exit = None;
        Ok(ControlResponse {
            ok: true,
            state: CellState::Running,
            message: format!("started cell {}", spec.id()),
            host_pid: Some(pid),
            launch: None,
            applications: None,
            application_icon: None,
            application_icons: None,
            diagnostics: None,
        })
    }

    fn stop(&mut self, peer_uid: u32) -> Result<ControlResponse, ControlError> {
        let Some(active) = self.active.as_ref() else {
            return Ok(self.success("cell is already stopped"));
        };
        authorize_uid(peer_uid, active.spec.host_uid)?;
        let mut active = self.active.take().expect("active cell was just checked");
        active.cell.stop().map_err(development_error)?;
        self.last_exit = None;
        Ok(self.success(format!("stopped cell {}", active.spec.id())))
    }

    fn restart(
        &mut self,
        peer_uid: u32,
        spec_path: &Path,
    ) -> Result<ControlResponse, ControlError> {
        load_authorized_spec(peer_uid, spec_path, &self.config.spec_directory)?;
        if self.active.is_some() {
            self.stop(peer_uid)?;
        }
        self.start(peer_uid, spec_path)
    }

    fn status(&self, peer_uid: u32) -> Result<ControlResponse, ControlError> {
        if let Some(active) = &self.active {
            authorize_uid(peer_uid, active.spec.host_uid)?;
            let (_, phase) = android_boot_snapshot(&active.spec)?;
            Ok(self.success(format!(
                "cell {} is running from {}; {}",
                active.spec.id(),
                active.spec_path.display(),
                phase.map_or_else(
                    || "Android is ready".to_owned(),
                    |phase| format!("Android boot phase: {phase}")
                )
            )))
        } else {
            Ok(self.success(self.last_exit.as_deref().unwrap_or("cell is stopped")))
        }
    }

    fn launch(
        &mut self,
        peer_uid: u32,
        spec_path: &Path,
        package: &str,
        component: Option<&str>,
        resolution: Option<&str>,
        user: u32,
    ) -> Result<ControlResponse, ControlError> {
        validate_package(package)?;
        if let Some(resolution) = resolution {
            parse_launch_resolution(resolution)?;
        }
        if let Some(component) = component {
            validate_component(component)?;
            if component
                .split_once('/')
                .is_none_or(|(owner, _)| owner != package)
            {
                return Err(ControlError::Invalid(format!(
                    "launcher component {component} does not belong to package {package}"
                )));
            }
        }
        validate_android_user(user)?;
        let active = self.require_active_cell(peer_uid, Some(spec_path))?;
        let started = Instant::now();
        let init_pid = active.ready_init(&self.progress)?;
        let ready_us = started.elapsed().as_micros();
        self.progress
            .report("Android is ready. Launching the application (up to 120 seconds)...");
        let output = launch_in_cell(init_pid, package, component, resolution, user)?;
        eprintln!(
            "droidloom-launch-host-timing package={package} user={user} ready_us={ready_us} total_us={}",
            started.elapsed().as_micros(),
        );
        Ok(ControlResponse {
            ok: true,
            state: CellState::Running,
            message: format!("launched {package}"),
            host_pid: Some(active.cell.host_pid()),
            launch: Some(output),
            applications: None,
            application_icon: None,
            application_icons: None,
            diagnostics: None,
        })
    }

    fn list_applications(&self, peer_uid: u32, user: u32) -> Result<ControlResponse, ControlError> {
        validate_android_user(user)?;
        let active = self.require_active_cell(peer_uid, None)?;
        self.progress
            .report("Android is ready. Reading the application list (up to 115 seconds)...");
        let init = active.ready_init(&self.progress)?;
        let output = framework_request(
            init,
            framework_wire::CATALOG,
            &serde_json::json!({"user":user}),
        )?;
        let applications: Vec<AndroidApplication> =
            serde_json::from_str(&output).map_err(|error| {
                ControlError::Invalid(format!(
                    "Android application catalog returned invalid JSON: {error}"
                ))
            })?;
        validate_application_catalog(&applications)?;
        Ok(ControlResponse {
            ok: true,
            state: CellState::Running,
            message: format!("listed {} Android applications", applications.len()),
            host_pid: Some(active.cell.host_pid()),
            launch: None,
            applications: Some(applications),
            application_icon: None,
            application_icons: None,
            diagnostics: None,
        })
    }

    fn install(
        &self,
        peer_uid: u32,
        user: u32,
        file: Option<fs::File>,
    ) -> Result<ControlResponse, ControlError> {
        validate_android_user(user)?;
        let active = self.require_active_cell(peer_uid, None)?;
        let file = file.ok_or_else(|| {
            ControlError::Invalid(
                "install requires an APK file descriptor; update both droidloomctl and droidloomd"
                    .into(),
            )
        })?;
        apk::validate(&file)?;
        let pid = active.ready_init(&self.progress)?;
        self.progress
            .report("Android is ready. Installing the APK (up to 115 seconds)...");
        apk::install(pid, user, file)?;
        Ok(self.success(
            "APK installed successfully; the application catalog will refresh automatically",
        ))
    }

    fn application_icon(
        &self,
        peer_uid: u32,
        component: &str,
        user: u32,
        size: u32,
    ) -> Result<ControlResponse, ControlError> {
        validate_component(component)?;
        validate_android_user(user)?;
        if !(MIN_ICON_SIZE..=MAX_ICON_SIZE).contains(&size) {
            return Err(ControlError::Invalid(format!(
                "application icon size must be between {MIN_ICON_SIZE} and {MAX_ICON_SIZE} pixels"
            )));
        }
        let active = self.require_active_cell(peer_uid, None)?;
        self.progress
            .report("Android is ready. Loading the application icon (up to 115 seconds)...");
        let init = active.ready_init(&self.progress)?;
        let output = framework_request(
            init,
            framework_wire::ICONS,
            &serde_json::json!({"user":user,"components":[component],"size":size}),
        )?;
        let icons: std::collections::BTreeMap<String, String> = serde_json::from_str(&output)?;
        let icon = icons
            .get(component)
            .ok_or_else(|| ControlError::Invalid("launcher icon unavailable".into()))?
            .clone();
        if icon.is_empty() || icon.len() as u64 > MAX_RESPONSE_BYTES / 2 {
            return Err(ControlError::Invalid(
                "Android application icon output has an invalid size".into(),
            ));
        }
        Ok(ControlResponse {
            ok: true,
            state: CellState::Running,
            message: format!("rendered Android application icon for {component}"),
            host_pid: Some(active.cell.host_pid()),
            launch: None,
            applications: None,
            application_icon: Some(icon),
            application_icons: None,
            diagnostics: None,
        })
    }

    fn application_icons(
        &self,
        peer_uid: u32,
        components: &[String],
        user: u32,
        size: u32,
    ) -> Result<ControlResponse, ControlError> {
        if user != 0
            || components.is_empty()
            || components.len() > 32
            || !(MIN_ICON_SIZE..=MAX_ICON_SIZE).contains(&size)
        {
            return Err(ControlError::Invalid(
                "invalid icon batch scope or size".into(),
            ));
        }
        for component in components {
            validate_component(component)?;
        }
        let active = self.require_active_cell(peer_uid, None)?;
        let init = active.ready_init(&self.progress)?;
        let result = framework_request(
            init,
            framework_wire::ICONS,
            &serde_json::json!({"user":user,"components":components,"size":size}),
        )?;
        let icons: std::collections::BTreeMap<String, String> = serde_json::from_str(&result)?;
        if icons.iter().any(|(name, png)| {
            !components.contains(name) || png.is_empty() || png.len() > 2 * 1024 * 1024
        }) {
            return Err(ControlError::Invalid(
                "framework returned invalid icon batch".into(),
            ));
        }
        let mut response = self.success("rendered Android launcher icon batch");
        response.application_icons = Some(icons);
        Ok(response)
    }

    fn watch_applications(&self, peer_uid: u32) -> Result<UnixStream, ControlError> {
        let active = self.require_active_cell(peer_uid, None)?;
        let init = match active.ready_init.get() {
            Some(pid) => pid,
            None => android_init_pid(&active.spec)?
                .ok_or_else(|| ControlError::Invalid("Android init unavailable".into()))?,
        };
        framework_control::connect(init, "catalog-events")
    }

    fn set_dpi(
        &mut self,
        peer_uid: u32,
        spec_path: &Path,
        dpi: u32,
    ) -> Result<ControlResponse, ControlError> {
        validate_display_density(dpi)?;
        self.ensure_active_cell(peer_uid, spec_path)?;
        let active = self
            .active
            .as_ref()
            .expect("density configuration ensured an active cell");
        let init_pid = active.ready_init(&self.progress)?;
        self.progress
            .report("Android is ready. Applying display density (up to 120 seconds)...");
        set_dpi_in_cell(init_pid, dpi)?;
        Ok(ControlResponse {
            ok: true,
            state: CellState::Running,
            message: format!("set persistent Android display density to {dpi} dpi"),
            host_pid: Some(active.cell.host_pid()),
            launch: None,
            applications: None,
            application_icon: None,
            application_icons: None,
            diagnostics: None,
        })
    }

    fn ensure_active_cell(&mut self, peer_uid: u32, spec_path: &Path) -> Result<(), ControlError> {
        if self.active.is_none() {
            self.start(peer_uid, spec_path)?;
            return Ok(());
        }
        let requested = load_authorized_spec(peer_uid, spec_path, &self.config.spec_directory)?;
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.spec != requested)
        {
            return Err(ControlError::Invalid(
                "the running cell does not match the requested specification".into(),
            ));
        }
        Ok(())
    }

    fn require_active_cell(
        &self,
        peer_uid: u32,
        expected_spec: Option<&Path>,
    ) -> Result<&ActiveCell, ControlError> {
        let active = self.active.as_ref().ok_or_else(|| {
            ControlError::Invalid("Droidloom's boot-managed Android cell is not running".into())
        })?;
        authorize_uid(peer_uid, active.spec.host_uid)?;
        if let Some(expected_spec) = expected_spec {
            let requested =
                load_authorized_spec(peer_uid, expected_spec, &self.config.spec_directory)?;
            if active.spec != requested {
                return Err(ControlError::Invalid(
                    "the running cell does not match the requested specification".into(),
                ));
            }
        }
        Ok(active)
    }

    fn diagnostics(
        &self,
        peer_uid: u32,
        package: Option<&str>,
        user: u32,
        lines: u32,
        crashes: bool,
    ) -> Result<ControlResponse, ControlError> {
        let active = self.require_active_cell(peer_uid, None)?;
        if let Some(package) = package {
            validate_package(package)?;
        }
        validate_android_user(user)?;
        if !(1..=2000).contains(&lines) {
            return Err(ControlError::Invalid(
                "log line count must be between 1 and 2000".into(),
            ));
        }
        // Diagnostics must remain available during a failed/incomplete boot.
        let pid = android_init_pid(&active.spec)?.ok_or_else(|| {
            ControlError::Invalid("Android init is not available for diagnostics".into())
        })?;
        let report = diagnostics::collect(pid, package, user, lines, crashes)?;
        let mut response = self.success(if crashes {
            "Android crash report"
        } else {
            "Android logs"
        });
        response.diagnostics = Some(report);
        Ok(response)
    }

    fn success(&self, message: impl Into<String>) -> ControlResponse {
        ControlResponse {
            ok: true,
            state: if self.active.is_some() {
                CellState::Running
            } else {
                CellState::Stopped
            },
            message: message.into(),
            host_pid: self.active.as_ref().map(|active| active.cell.host_pid()),
            launch: None,
            applications: None,
            application_icon: None,
            application_icons: None,
            diagnostics: None,
        }
    }

    fn failure(&self, message: impl Into<String>) -> ControlResponse {
        let mut response = self.success(message);
        response.ok = false;
        response
    }

    fn shutdown(&mut self) -> Result<(), ControlError> {
        if let Some(mut active) = self.active.take() {
            active.cell.stop().map_err(development_error)?;
        }
        Ok(())
    }
}

/// Run the lifecycle daemon until `shutdown` is requested by a signal handler.
///
/// # Errors
///
/// Returns socket setup, protocol I/O, cell lifecycle, or final cleanup
/// failures.
pub fn serve(config: &DaemonConfig, shutdown: &Shutdown) -> Result<(), ControlError> {
    let listener = bind_listener(&config.socket)?;
    listener
        .set_nonblocking(true)
        .map_err(|source| io_error("make lifecycle listener nonblocking", source))?;
    let mut daemon = Daemon::new(config.clone());
    let loop_result = run_loop(&listener, &mut daemon, shutdown);
    let shutdown_result = daemon.shutdown();
    drop(listener);
    let socket_result = remove_socket(&config.socket);
    loop_result.and(shutdown_result).and(socket_result)
}

fn run_loop(
    listener: &UnixListener,
    daemon: &mut Daemon,
    shutdown: &Shutdown,
) -> Result<(), ControlError> {
    let mut sources = WaitSources::default();
    loop {
        daemon.poll()?;
        sources
            .child(daemon.active.as_ref().map(|active| active.cell.host_pid()))
            .map_err(|source| io_error("watch lifecycle child", source))?;
        if !sources
            .wait_with_fd(
                listener,
                shutdown,
                daemon
                    .pen_input_watcher
                    .as_ref()
                    .map(|watcher| watcher.fd.as_raw_fd()),
            )
            .map_err(|source| io_error("wait for lifecycle readiness", source))?
        {
            return Ok(());
        }
        daemon.poll()?;
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut delegated = None;
                let response = match receive_request(&mut stream) {
                    Ok((peer_uid, request, file, streaming)) => {
                        daemon.progress = Reporter::new(&stream, streaming)?;
                        if matches!(request, ControlRequest::WatchApplications) {
                            match daemon.watch_applications(peer_uid) {
                                Ok(events) => {
                                    delegated =
                                        Some(fs::File::from(std::os::fd::OwnedFd::from(events)));
                                    daemon.success("subscribed to framework catalog events")
                                }
                                Err(error) => daemon.failure(error.to_string()),
                            }
                        } else {
                            daemon.handle(peer_uid, request, file)
                        }
                    }
                    Err(error) => daemon.failure(error.to_string()),
                };
                // Closing a diagnostic client must not tear down Android.
                let delivered = if let Some(file) = delegated {
                    apk::send_file(&stream, 1, &file)
                } else {
                    send_response(&mut stream, &response)
                };
                if let Err(error) = delivered {
                    eprintln!("could not deliver lifecycle response: {error}");
                }
                daemon.progress = Reporter::default();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                continue;
            }
            Err(source) => return Err(io_error("accept lifecycle client", source)),
        }
    }
}

/// Send one request and wait for its bounded response.
///
/// # Errors
///
/// Returns connection, I/O, size-bound, or JSON failures.
pub fn request(socket: &Path, request: &ControlRequest) -> Result<ControlResponse, ControlError> {
    progress::request(socket, request, None, None)
}

/// Send a lifecycle request, reporting boot stages and elapsed waiting time.
///
/// # Errors
/// Returns bounded transport or protocol failures. Requires a progress-capable daemon.
pub fn request_with_progress(
    socket: &Path,
    request: &ControlRequest,
    report: &mut dyn FnMut(&str),
) -> Result<ControlResponse, ControlError> {
    progress::request(socket, request, None, Some(report))
}

/// Install a standalone APK opened with the calling user's permissions.
///
/// # Errors
/// Returns invalid-file, transport, or protocol failures. Android failures are
/// returned in the control response.
pub fn install_apk(socket: &Path, path: &Path, user: u32) -> Result<ControlResponse, ControlError> {
    install_apk_reporting(socket, path, user, None)
}

/// Install an APK with boot and installation progress and a bounded wait.
///
/// # Errors
/// Returns invalid-file, transport, or protocol failures.
pub fn install_apk_with_progress(
    socket: &Path,
    path: &Path,
    user: u32,
    report: &mut dyn FnMut(&str),
) -> Result<ControlResponse, ControlError> {
    install_apk_reporting(socket, path, user, Some(report))
}

fn install_apk_reporting(
    socket: &Path,
    path: &Path,
    user: u32,
    report: Option<&mut dyn FnMut(&str)>,
) -> Result<ControlResponse, ControlError> {
    use std::os::unix::fs::OpenOptionsExt;
    validate_android_user(user)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|source| io_error(&format!("open APK {}", path.display()), source))?;
    apk::validate(&file)?;
    progress::request(
        socket,
        &ControlRequest::Install { user },
        Some(&file),
        report,
    )
}

fn bind_listener(socket: &Path) -> Result<UnixListener, ControlError> {
    let parent = socket.parent().ok_or_else(|| {
        ControlError::Invalid("lifecycle socket must have a parent directory".into())
    })?;
    fs::create_dir_all(parent)
        .map_err(|source| io_error("create lifecycle runtime directory", source))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o755))
        .map_err(|source| io_error("set lifecycle runtime directory mode", source))?;

    match fs::symlink_metadata(socket) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(ControlError::Invalid(format!(
                    "refusing to replace non-socket lifecycle path {}",
                    socket.display()
                )));
            }
            if UnixStream::connect(socket).is_ok() {
                return Err(ControlError::Invalid(format!(
                    "another droidloomd is listening on {}",
                    socket.display()
                )));
            }
            remove_socket(socket)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error("inspect lifecycle socket", source)),
    }

    let listener =
        UnixListener::bind(socket).map_err(|source| io_error("bind lifecycle socket", source))?;
    // Authorization is enforced with unforgeable SO_PEERCRED. World-connect
    // avoids forcing a distribution-specific group or polkit policy merely to
    // let the owning interactive UID control its own cell.
    fs::set_permissions(socket, fs::Permissions::from_mode(0o666))
        .map_err(|source| io_error("set lifecycle socket mode", source))?;
    Ok(listener)
}

fn receive_request(
    stream: &mut UnixStream,
) -> Result<(u32, ControlRequest, Option<fs::File>, bool), ControlError> {
    stream
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .map_err(|source| io_error("set lifecycle request timeout", source))?;
    stream
        .set_write_timeout(Some(CLIENT_TIMEOUT))
        .map_err(|source| io_error("set lifecycle response write timeout", source))?;
    let peer_uid = peer_uid(stream)?;
    let (first, file) = apk::receive_file(stream)?;
    let mut encoded = vec![first];
    stream
        .take(MAX_MESSAGE_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(|source| io_error("read lifecycle request", source))?;
    if encoded.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(ControlError::Invalid(
            "lifecycle request is too large".into(),
        ));
    }
    let (request, streaming) = match serde_json::from_slice(&encoded)? {
        progress::IncomingRequest::Progress(envelope) => (envelope.request, true),
        progress::IncomingRequest::Legacy(request) => (request, false),
    };
    if file.is_some() && !matches!(request, ControlRequest::Install { .. }) {
        return Err(ControlError::Invalid(
            "only install accepts a file descriptor".into(),
        ));
    }
    Ok((peer_uid, request, file, streaming))
}

fn send_response(stream: &mut UnixStream, response: &ControlResponse) -> Result<(), ControlError> {
    let mut encoded = serde_json::to_vec(response)?;
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .map_err(|source| io_error("send lifecycle response", source))
}

fn peer_uid(stream: &UnixStream) -> Result<u32, ControlError> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
        .expect("ucred size fits socklen_t");
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut credentials).cast(),
            std::ptr::from_mut(&mut length),
        )
    };
    if result != 0 {
        return Err(io_error(
            "read lifecycle peer credentials",
            io::Error::last_os_error(),
        ));
    }
    Ok(credentials.uid)
}

fn load_spec(path: &Path) -> Result<CellSpec, ControlError> {
    let metadata =
        fs::metadata(path).map_err(|source| io_error("stat cell specification", source))?;
    if metadata.len() > MAX_SPEC_BYTES {
        return Err(ControlError::Invalid(format!(
            "cell specification is {} bytes; maximum is {MAX_SPEC_BYTES}",
            metadata.len()
        )));
    }
    let spec: CellSpec = serde_json::from_slice(
        &fs::read(path).map_err(|source| io_error("read cell specification", source))?,
    )?;
    spec.validate()
        .map_err(|error| ControlError::Invalid(error.to_string()))?;
    Ok(spec)
}

fn load_authorized_spec(
    peer_uid: u32,
    path: &Path,
    spec_directory: &Path,
) -> Result<CellSpec, ControlError> {
    if peer_uid != 0 {
        let metadata = fs::symlink_metadata(path)
            .map_err(|source| io_error("inspect cell specification", source))?;
        let canonical = fs::canonicalize(path)
            .map_err(|source| io_error("resolve cell specification", source))?;
        let canonical_directory = fs::canonicalize(spec_directory)
            .map_err(|source| io_error("resolve trusted specification directory", source))?;
        if !metadata.file_type().is_file()
            || metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
            || !canonical.starts_with(&canonical_directory)
        {
            return Err(ControlError::Invalid(format!(
                "non-root clients may use only root-owned, non-writable specifications below {}",
                spec_directory.display()
            )));
        }
    }
    let spec = load_spec(path)?;
    authorize_spec(peer_uid, &spec)?;
    Ok(spec)
}

fn authorize_spec(peer_uid: u32, spec: &CellSpec) -> Result<(), ControlError> {
    authorize_uid(peer_uid, spec.host_uid)
}

fn authorize_uid(peer_uid: u32, owner_uid: u32) -> Result<(), ControlError> {
    if peer_uid == 0 || peer_uid == owner_uid {
        Ok(())
    } else {
        Err(ControlError::Invalid(format!(
            "host UID {peer_uid} is not authorized for host UID {owner_uid}'s cell"
        )))
    }
}

fn validate_package(package: &str) -> Result<(), ControlError> {
    let valid = !package.is_empty()
        && package.len() <= 255
        && package.split('.').count() >= 2
        && package
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_'));
    if valid {
        Ok(())
    } else {
        Err(ControlError::Invalid(format!(
            "invalid Android package name {package:?}"
        )))
    }
}

fn validate_component(component: &str) -> Result<(), ControlError> {
    if component.len() <= 511
        && component
            .split_once('/')
            .is_some_and(|(package, activity)| {
                validate_package(package).is_ok()
                    && !activity.is_empty()
                    && activity.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'$')
                    })
            })
    {
        Ok(())
    } else {
        Err(ControlError::Invalid(format!(
            "invalid flattened Android component {component:?}"
        )))
    }
}

fn validate_android_user(user: u32) -> Result<(), ControlError> {
    if user <= i32::MAX as u32 / ANDROID_PER_USER_UID_RANGE {
        Ok(())
    } else {
        Err(ControlError::Invalid(format!(
            "Android user ID {user} exceeds the framework range"
        )))
    }
}

fn validate_application_catalog(applications: &[AndroidApplication]) -> Result<(), ControlError> {
    if applications.len() > MAX_APPLICATIONS {
        return Err(ControlError::Invalid(format!(
            "Android returned {} launchable activities; maximum is {MAX_APPLICATIONS}",
            applications.len()
        )));
    }
    let mut components = std::collections::BTreeSet::new();
    for application in applications {
        validate_package(&application.package)?;
        validate_component(&application.component)?;
        if application
            .component
            .split_once('/')
            .is_none_or(|(package, _)| package != application.package)
        {
            return Err(ControlError::Invalid(format!(
                "launcher component {} does not belong to package {}",
                application.component, application.package
            )));
        }
        if application.name.is_empty()
            || application.name.len() > 1024
            || application.name.contains('\0')
        {
            return Err(ControlError::Invalid(format!(
                "launcher {} has an invalid label",
                application.component
            )));
        }
        if application.icon_key.is_empty()
            || application.icon_key.len() > 128
            || !application.icon_key.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':')
            })
        {
            return Err(ControlError::Invalid(format!(
                "launcher {} has an invalid icon identity",
                application.component
            )));
        }
        if !components.insert(&application.component) {
            return Err(ControlError::Invalid(format!(
                "Android returned duplicate launcher {}",
                application.component
            )));
        }
    }
    Ok(())
}

fn wait_for_android_init(spec: &CellSpec, progress: &Reporter) -> Result<u32, ControlError> {
    wait_for_boot(
        ANDROID_READY_TIMEOUT,
        Duration::from_millis(100),
        || android_boot_snapshot(spec),
        |message| progress.report(message),
    )
}

fn wait_for_boot(
    timeout: Duration,
    interval: Duration,
    mut observe: impl FnMut() -> Result<(Option<u32>, Option<String>), ControlError>,
    mut report: impl FnMut(&str),
) -> Result<u32, ControlError> {
    let deadline = Instant::now() + timeout;
    let mut last_phase = String::new();
    report(&format!(
        "Waiting for Android to complete boot (up to {} seconds)...",
        timeout.as_secs()
    ));
    while Instant::now() < deadline {
        let (pid, phase) = observe()?;
        let Some(phase) = phase else {
            return pid.ok_or_else(|| {
                ControlError::Invalid(
                    "Android readiness probe did not return its init process".into(),
                )
            });
        };
        if phase != last_phase {
            report(&format!("Android boot phase: {phase}."));
            last_phase = phase;
        }
        thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
    }
    Err(ControlError::Invalid(format!(
        "Android looks stuck: it did not become ready within {} seconds.\nLast boot phase: {last_phase}. First boot can take longer while Android initializes its data.\n{LOG_HINT}\nUse `droidloomctl wait` to check readiness again without restarting Android.",
        timeout.as_secs()
    )))
}

fn android_boot_snapshot(spec: &CellSpec) -> Result<(Option<u32>, Option<String>), ControlError> {
    let Some(pid) = android_init_pid(spec)? else {
        return Ok((None, Some("waiting for Android init".into())));
    };
    let launcher = PathBuf::from(format!(
        "/proc/{pid}/root/dev/socket/droidloom-framework/control"
    ));
    let task_control = PathBuf::from(format!(
        "/proc/{pid}/root/dev/socket/droidloom-task-control"
    ));
    let control_ready =
        fs::symlink_metadata(task_control).is_ok_and(|metadata| metadata.file_type().is_socket());
    let properties = android_properties(pid)?;
    Ok((
        Some(pid),
        properties.map_or_else(
            || Some("waiting for Android's property service to respond".into()),
            |properties| android_boot_phase(&properties, launcher.exists(), control_ready),
        ),
    ))
}

fn android_properties(init_pid: u32) -> Result<Option<Vec<u8>>, ControlError> {
    let pid = init_pid.to_string();
    let output = droidloom_cpu_placement::command("timeout")
        .args(["--kill-after=1s", "5s", "nsenter"])
        .args([
            "--target",
            &pid,
            "--mount",
            "--uts",
            "--ipc",
            "--net",
            "--pid",
            "--cgroup",
            "--root",
            "--wd",
            "--",
            "/system/bin/getprop",
        ])
        .output()
        .map_err(|source| io_error("read Android readiness properties", source))?;
    Ok(output.status.success().then_some(output.stdout))
}

fn android_boot_phase(properties: &[u8], launcher: bool, task_control: bool) -> Option<String> {
    let properties = String::from_utf8_lossy(properties);
    let property = |name: &str| {
        let prefix = format!("[{name}]: [");
        properties
            .lines()
            .find_map(|line| line.strip_prefix(&prefix)?.strip_suffix(']'))
    };
    // Show observed service states, not invented percentages or framework phases.
    let service = |name| {
        property(name)
            .filter(|value| matches!(*value, "running" | "stopped" | "restarting" | "stopping"))
            .unwrap_or("not reported")
    };
    if property("sys.boot_completed") != Some("1") {
        Some(format!(
            "waiting for Android to complete boot (Android runtime: {}; boot animation: {})",
            service("init.svc.zygote"),
            service("init.svc.bootanim")
        ))
    } else if property("sys.droidloom.framework_ready") != Some("1") {
        Some(format!(
            "Android boot completed; waiting for Droidloom framework input/channels ({})",
            property("sys.droidloom.framework_ready").unwrap_or("not reported")
        ))
    } else if !launcher {
        Some("Android boot completed; waiting for Droidloom native framework endpoint".into())
    } else if !task_control {
        Some("Android boot completed; waiting for Droidloom task control".into())
    } else {
        None
    }
}

fn android_init_pid(spec: &CellSpec) -> Result<Option<u32>, ControlError> {
    for pid in android_namespace_pids(spec)? {
        let status = match fs::read_to_string(format!("/proc/{pid}/status")) {
            Ok(status) => status,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_error("read cell process namespace IDs", source)),
        };
        let is_init = status
            .lines()
            .find_map(|line| line.strip_prefix("NSpid:"))
            .and_then(|ids| ids.split_whitespace().last())
            == Some("1");
        if is_init {
            return Ok(Some(pid));
        }
    }
    Ok(None)
}

fn android_namespace_pids(spec: &CellSpec) -> Result<Vec<u32>, ControlError> {
    let namespace = format!("droidloom-u{}", spec.host_uid);
    let output = droidloom_cpu_placement::command("timeout")
        .args(["--kill-after=1s", "5s", "ip", "netns", "pids", &namespace])
        .output()
        .map_err(|source| io_error("list exact cell namespace processes", source))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|pid| pid.parse::<u32>().ok())
        .collect())
}

/// Validate a launch extent in Android pixels, with no shell metacharacters.
///
/// # Errors
/// Rejects malformed, zero, or oversized dimensions.
pub fn parse_launch_resolution(value: &str) -> Result<String, ControlError> {
    let valid = value.split_once('x').is_some_and(|(width, height)| {
        [width, height].iter().all(|dimension| {
            !dimension.is_empty()
                && dimension.len() <= 5
                && dimension.bytes().all(|byte| byte.is_ascii_digit())
                && dimension
                    .parse::<u32>()
                    .is_ok_and(|size| (1..=16_384).contains(&size))
        })
    });
    if !valid {
        return Err(ControlError::Invalid(
            "resolution must be WIDTHxHEIGHT with each dimension in 1..=16384".into(),
        ));
    }
    Ok(value.to_owned())
}

fn android_operation_timeout(operation: &str) -> ControlError {
    ControlError::Invalid(format!(
        "Android looks stuck: {operation} did not finish before its deadline.\n{LOG_HINT}"
    ))
}

fn validate_display_density(dpi: u32) -> Result<(), ControlError> {
    if dpi < MIN_DISPLAY_DPI || dpi > i32::MAX as u32 {
        return Err(ControlError::Invalid(format!(
            "display density must be between {MIN_DISPLAY_DPI} and {} dpi",
            i32::MAX
        )));
    }
    Ok(())
}

fn remove_socket(socket: &Path) -> Result<(), ControlError> {
    match fs::remove_file(socket) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error("remove lifecycle socket", source)),
    }
}

fn development_error(error: impl std::fmt::Display) -> ControlError {
    ControlError::Invalid(error.to_string())
}

fn io_error(context: &str, source: io::Error) -> ControlError {
    ControlError::Io {
        context: context.into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pen_input_watch_keeps_event_node_changes_and_queue_overflow() {
        let mut bytes = Vec::new();
        let name = b"event12\0";
        bytes.extend_from_slice(&1_i32.to_ne_bytes());
        bytes.extend_from_slice(&libc::IN_DELETE.to_ne_bytes());
        bytes.extend_from_slice(&0_u32.to_ne_bytes());
        bytes.extend_from_slice(&(name.len() as u32).to_ne_bytes());
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&(-1_i32).to_ne_bytes());
        bytes.extend_from_slice(&libc::IN_Q_OVERFLOW.to_ne_bytes());
        bytes.extend_from_slice(&0_u32.to_ne_bytes());
        bytes.extend_from_slice(&0_u32.to_ne_bytes());

        assert_eq!(
            parse_pen_input_events(&bytes),
            [
                (libc::IN_DELETE, "event12".to_owned()),
                (libc::IN_Q_OVERFLOW, String::new())
            ]
        );
        assert!(!is_host_event_name("by-path"));
        assert!(!is_host_event_name("event12x"));
    }

    use std::sync::Arc;

    #[test]
    fn install_transport_delivers_open_file_and_preserves_android_failure() {
        for use_progress in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("install.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let path = directory.path().join("app with spaces.apk");
            fs::write(&path, b"PK\x03\x04test payload").unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let (_, request, file, streaming) = receive_request(&mut stream).unwrap();
                assert_eq!(streaming, use_progress);
                assert_eq!(request, ControlRequest::Install { user: 10 });
                assert_eq!(apk::validate(&file.unwrap()).unwrap(), 16);
                Reporter::new(&stream, streaming)
                    .unwrap()
                    .report("Android is ready. Installing the APK...");
                send_response(
                    &mut stream,
                    &ControlResponse {
                        ok: false,
                        state: CellState::Running,
                        message: "Failure [INSTALL_FAILED_NO_MATCHING_ABIS]".into(),
                        host_pid: None,
                        launch: None,
                        applications: None,
                        application_icon: None,
                        application_icons: None,
                        diagnostics: None,
                    },
                )
                .unwrap();
            });
            let mut messages = Vec::new();
            let response = if use_progress {
                install_apk_with_progress(&socket, &path, 10, &mut |message| {
                    messages.push(message.to_owned());
                })
            } else {
                install_apk(&socket, &path, 10)
            }
            .unwrap();
            assert_eq!(
                messages
                    .iter()
                    .any(|message| message.contains("Installing the APK")),
                use_progress
            );
            assert!(!response.ok);
            assert!(response.message.contains("INSTALL_FAILED_NO_MATCHING_ABIS"));
            server.join().unwrap();
        }
    }

    #[test]
    fn install_missing_file_fails_before_connecting() {
        let directory = tempfile::tempdir().unwrap();
        let error = install_apk(
            &directory.path().join("absent.sock"),
            &directory.path().join("absent.apk"),
            0,
        )
        .unwrap_err();
        assert!(error.to_string().contains("open APK"));
    }

    #[test]
    fn protocol_round_trip_is_explicit() {
        let request = ControlRequest::Launch {
            spec: "/etc/droidloom/cell.json".into(),
            package: "org.mozilla.firefox".into(),
            component: Some("org.mozilla.firefox/.App".into()),
            resolution: Some("2560x1440".into()),
            user: 0,
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            serde_json::from_slice::<ControlRequest>(&encoded).unwrap(),
            request
        );
    }

    #[test]
    fn launch_resolution_validation_and_old_clients() {
        assert_eq!(parse_launch_resolution("2560x1440").unwrap(), "2560x1440");
        for invalid in [
            "0x1440",
            "2560",
            "-1x1440",
            "2560x1440x2",
            "16385x1080",
            "1x+2",
            "1x2;id",
        ] {
            assert!(parse_launch_resolution(invalid).is_err(), "{invalid}");
        }
        let old = br#"{"command":"launch","spec":"/etc/droidloom/cell.json","package":"org.example.game","component":null,"user":0}"#;
        // Older clients omit the new optional field.
        let request: ControlRequest = serde_json::from_slice(old).unwrap();
        assert!(matches!(
            request,
            ControlRequest::Launch {
                resolution: None,
                ..
            }
        ));
    }

    #[test]
    fn persistent_density_protocol_round_trip_is_explicit() {
        let request = ControlRequest::SetDpi {
            spec: "/etc/droidloom/cell.json".into(),
            dpi: 175,
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            serde_json::from_slice::<ControlRequest>(&encoded).unwrap(),
            request
        );
        assert!(validate_display_density(175).is_ok());
        assert!(validate_display_density(71).is_err());
    }

    #[test]
    fn boot_complete_does_not_hide_a_failed_input_service() {
        assert!(android_boot_phase(b"[sys.boot_completed]: [1]\n", true, true).is_some());
        assert!(
            android_boot_phase(
                b"[sys.boot_completed]: [1]\n[sys.droidloom.framework_ready]: [0]\n",
                true,
                true
            )
            .unwrap()
            .contains("framework input/channels (0)")
        );
        assert!(
            android_boot_phase(
                b"[sys.boot_completed]: [1]\n[sys.droidloom.framework_ready]: [1]\n",
                true,
                true
            )
            .is_none()
        );
    }

    #[test]
    fn boot_progress_uses_observed_services_and_checks_every_readiness_gate() {
        let booting = b"[init.svc.zygote]: [restarting]\n[init.svc.bootanim]: [running]\n";
        let phase = android_boot_phase(booting, true, true).unwrap();
        assert!(phase.contains("complete boot"));
        assert!(phase.contains("Android runtime: restarting"));
        assert!(phase.contains("boot animation: running"));
        let ready = b"[sys.boot_completed]: [1]\n[sys.droidloom.framework_ready]: [1]\n";
        assert!(
            android_boot_phase(ready, false, true)
                .unwrap()
                .contains("native framework endpoint")
        );
        assert!(
            android_boot_phase(ready, true, false)
                .unwrap()
                .contains("task control")
        );
        assert!(android_boot_phase(ready, true, true).is_none());
    }

    #[test]
    fn boot_wait_reports_changes_and_a_stall_has_the_last_phase_and_log_commands() {
        let mut observations = [
            (None, Some("waiting for Android init".into())),
            (
                Some(42),
                Some("waiting for Android to complete boot".into()),
            ),
            (
                Some(42),
                Some("waiting for Android to complete boot".into()),
            ),
            (Some(42), None),
        ]
        .into_iter();
        let mut messages = Vec::new();
        assert_eq!(
            wait_for_boot(
                Duration::from_secs(1),
                Duration::ZERO,
                || Ok(observations.next().unwrap()),
                |message| messages.push(message.to_owned())
            )
            .unwrap(),
            42
        );
        assert_eq!(
            messages.len(),
            3,
            "unchanged phases must not flood the terminal"
        );
        let error = wait_for_boot(
            Duration::from_millis(20),
            Duration::from_millis(1),
            || {
                Ok((
                    Some(42),
                    Some("waiting for Droidloom input service (restarting)".into()),
                ))
            },
            |_| {},
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Android looks stuck"));
        assert!(
            error.contains("Last boot phase: waiting for Droidloom input service (restarting)")
        );
        assert!(error.contains("journalctl -b -u droidloomd.service"));
        assert!(error.contains("droidloomctl wait"));
    }
    #[test]
    fn application_catalog_protocol_round_trip_is_explicit() {
        let request = ControlRequest::ApplicationIcon {
            component: "org.mozilla.firefox/.App".into(),
            user: 0,
            size: 128,
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            serde_json::from_slice::<ControlRequest>(&encoded).unwrap(),
            request
        );
        assert!(
            validate_application_catalog(&[AndroidApplication {
                name: "Firefox".into(),
                package: "org.mozilla.firefox".into(),
                component: "org.mozilla.firefox/.App".into(),
                icon_key: "125:42:17301504".into(),
            }])
            .is_ok()
        );
    }

    #[test]
    fn package_and_component_validation_reject_shell_syntax() {
        assert!(validate_package("org.mozilla.firefox").is_ok());
        assert!(validate_package("org.mozilla.firefox;reboot").is_err());
        assert!(validate_component("org.mozilla.firefox/.App").is_ok());
        assert!(validate_component("org.mozilla.firefox/.App --user 10").is_err());
    }

    #[test]
    fn only_root_or_the_exact_cell_owner_is_authorized() {
        assert!(authorize_uid(0, 1000).is_ok());
        assert!(authorize_uid(1000, 1000).is_ok());
        assert!(authorize_uid(1001, 1000).is_err());
    }

    #[test]
    fn local_daemon_status_and_stopped_teardown_are_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.sock");
        let config = DaemonConfig {
            socket: socket.clone(),
            supervisor: directory.path().join("unused-supervisor"),
            spec_directory: directory.path().join("unused-specifications"),
        };
        let shutdown = Arc::new(Shutdown::new().unwrap());
        let daemon_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || serve(&config, &daemon_shutdown));

        let deadline = Instant::now() + Duration::from_secs(2);
        while !socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !socket.exists() {
            shutdown.request().unwrap();
            let result = thread.join().unwrap();
            if matches!(
                result,
                Err(ControlError::Io {
                    ref source,
                    ..
                }) if source.kind() == io::ErrorKind::PermissionDenied
            ) {
                // Some build sandboxes deny AF_UNIX bind entirely. The same
                // test exercises the full exchange on ordinary Linux hosts.
                return;
            }
            panic!("daemon did not publish its socket: {result:?}");
        }

        let status = request(&socket, &ControlRequest::Status).unwrap();
        assert!(status.ok);
        assert_eq!(status.state, CellState::Stopped);
        let apk_path = directory.path().join("app.apk");
        fs::write(&apk_path, b"PK\x03\x04test payload").unwrap();
        let install = install_apk(&socket, &apk_path, 0).unwrap();
        assert!(!install.ok);
        assert!(install.message.contains("not running"));
        // A cancelled report does not stop the daemon, nor start a cold cell.
        let diagnostics = ControlRequest::Diagnostics {
            package: None,
            user: 0,
            lines: 200,
            crashes: true,
        };
        let mut cancelled = UnixStream::connect(&socket).unwrap();
        cancelled
            .write_all(&serde_json::to_vec(&diagnostics).unwrap())
            .unwrap();
        cancelled.shutdown(std::net::Shutdown::Both).unwrap();
        drop(cancelled);
        let report = request(&socket, &diagnostics).unwrap();
        assert!(!report.ok);
        assert_eq!(report.state, CellState::Stopped);
        assert!(report.message.contains("not running"));
        for _ in 0..2 {
            let stopped = request(&socket, &ControlRequest::Stop).unwrap();
            assert!(stopped.ok);
            assert_eq!(stopped.state, CellState::Stopped);
        }

        shutdown.request().unwrap();
        thread.join().unwrap().unwrap();
        assert!(!socket.exists());
    }
}
