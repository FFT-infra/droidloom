//! Privileged Android-side task launch and binding primitive.
//!
//! The host supervisor executes this binary inside the Android cell as the
//! system or root identity. It starts exactly one package on Android's built-in
//! display, discovers its real `ActivityTaskManager` identity, then authorizes
//! only that task for direct `SurfaceFlinger` export. Android HOME and system
//! shell tasks consequently never acquire host windows.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

mod policy;

use droidloom_denial_ipc::{IpcError, SeqPacket};
use droidloom_task_control::{
    CodecError, ControlRequest, ControlResponse, PROTOCOL_MAJOR, decode_response, encode_request,
};

/// Android init path of Composer's root/system-only task-control socket.
pub const DEFAULT_CONTROL_SOCKET: &str = "/dev/socket/droidloom-task-control";
/// Android command-service client used instead of reimplementing framework Binder parcels.
pub const DEFAULT_ANDROID_COMMAND: &str = "/system/bin/cmd";

const ACTIVITY_DISCOVERY_ATTEMPTS: usize = 100;
const DISCOVERY_INTERVAL: Duration = Duration::from_millis(50);
const MAX_DIAGNOSTIC_BYTES: usize = 8_192;
const MIN_DISPLAY_DPI: u32 = 72;
#[cfg(target_os = "android")]
const AID_SHELL: u32 = 2_000;

/// One explicit package launch request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchRequest {
    /// Android package to launch and bind.
    pub package: String,
    /// Optional flattened component. When absent Android resolves the package's launcher activity.
    pub component: Option<String>,
    /// Android user identifier.
    pub user: u32,
}

/// Identities committed by a successful two-phase launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskBinding {
    /// Droidloom object identity.
    pub object: u64,
    /// Stable display handle owned by the Droidloom Composer service.
    pub composer_display: u64,
    /// Logical display identity assigned asynchronously by Android.
    pub android_display: u64,
    /// Real Android task identity.
    pub task: u64,
}

/// Launch, activity-service, discovery, or task-control failure.
#[derive(Debug)]
pub enum LaunchError {
    /// Invalid package, component, user, or command input.
    InvalidRequest(String),
    /// Local process operation failed.
    Io(std::io::Error),
    /// Sequenced-packet operation failed.
    Ipc(IpcError),
    /// Task-control record was malformed or incompatible.
    Codec(CodecError),
    /// Task-control service rejected or contradicted the request.
    Control(String),
    /// Android's activity command returned failure.
    Activity(String),
    /// The real task could not be identified unambiguously on its display.
    Discovery(String),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(formatter, "invalid launch request: {message}"),
            Self::Io(error) => write!(formatter, "process operation failed: {error}"),
            Self::Ipc(error) => write!(formatter, "task-control transport failed: {error}"),
            Self::Codec(error) => write!(formatter, "task-control codec failed: {error}"),
            Self::Control(message) => write!(formatter, "task-control request failed: {message}"),
            Self::Activity(message) => {
                write!(formatter, "Android activity launch failed: {message}")
            }
            Self::Discovery(message) => {
                write!(formatter, "Android task discovery failed: {message}")
            }
        }
    }
}

impl Error for LaunchError {}

impl From<std::io::Error> for LaunchError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<IpcError> for LaunchError {
    fn from(error: IpcError) -> Self {
        Self::Ipc(error)
    }
}

impl From<CodecError> for LaunchError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

/// Launch, discover, and authorize one Android task.
///
/// # Errors
///
/// Returns an error for invalid input, unavailable Android services, launch
/// failure, ambiguous task identity, or a rejected lifecycle transaction.
pub fn launch(request: &LaunchRequest) -> Result<TaskBinding, LaunchError> {
    launch_with_policy_cache(
        request,
        Path::new(DEFAULT_CONTROL_SOCKET),
        Path::new(DEFAULT_ANDROID_COMMAND),
        Some(Path::new("/dev/droidloom-launcher-policy")),
    )
}

/// Bind a task already resolved by the privileged Android task observer.
/// This performs no activity start, so intent extras, URI grants, results and
/// Android's original task selection remain intact.
///
/// # Errors
/// Rejects invalid identities or a failed task-control transaction.
pub fn bind_existing_task(package: &str, user: u32, task: u64) -> Result<TaskBinding, LaunchError> {
    validate_request(&LaunchRequest {
        package: package.into(),
        component: None,
        user,
    })?;
    if user != 0 || task == 0 || task > i32::MAX as u64 {
        return Err(LaunchError::InvalidRequest(
            "observed task must belong to user 0 and have a positive Android task ID".into(),
        ));
    }
    TaskControlClient::connect(Path::new(DEFAULT_CONTROL_SOCKET))?.register_direct(package, task, 0)
}

/// Persist a density override for Android's single built-in display.
///
/// Android stores this through `WindowManager` in its writable data image, so
/// the value survives Droidloom cell and host restarts.
///
/// # Errors
///
/// Rejects densities below Android's framework minimum or values that exceed
/// its signed command interface, and propagates command-service failures.
pub fn set_display_density(dpi: u32) -> Result<(), LaunchError> {
    set_display_density_with_path(dpi, Path::new(DEFAULT_ANDROID_COMMAND))
}

/// Set the persistent density through an explicit Android command path.
///
/// This is public primarily for controlled integration tests.
///
/// # Errors
/// Returns an error for invalid density values or failed Android commands.
pub fn set_display_density_with_path(dpi: u32, android_command: &Path) -> Result<(), LaunchError> {
    validate_display_density(dpi)?;
    let density = dpi.to_string();
    let output = command_as_android_shell(android_command)
        .args(["window", "density", &density, "-d", "0"])
        .output()?;
    checked_activity_output(&output, "set persistent built-in-display density")?;
    Ok(())
}

/// Launch with explicit paths, primarily for controlled integration tests.
///
/// # Errors
///
/// Has the same failure contract as [`launch`].
pub fn launch_with_paths(
    request: &LaunchRequest,
    control_socket: &Path,
    android_command: &Path,
) -> Result<TaskBinding, LaunchError> {
    launch_with_policy_cache(request, control_socket, android_command, None)
}

fn launch_with_policy_cache(
    request: &LaunchRequest,
    control_socket: &Path,
    android_command: &Path,
    cache_directory: Option<&Path>,
) -> Result<TaskBinding, LaunchError> {
    validate_request(request)?;
    let mut timing = LaunchTiming::new();
    let mut control = TaskControlClient::connect(control_socket)?;
    timing.record("control");
    let result = launch_direct(
        request,
        android_command,
        &mut control,
        cache_directory,
        &mut timing,
    );
    timing.finish(result.is_ok());
    result
}

fn launch_direct(
    request: &LaunchRequest,
    android_command: &Path,
    control: &mut TaskControlClient,
    cache_directory: Option<&Path>,
    timing: &mut LaunchTiming,
) -> Result<TaskBinding, LaunchError> {
    const BUILT_IN_DISPLAY: u64 = 0;
    timing.policy_cached = if let Some(directory) = cache_directory {
        policy::configure_once(android_command, request.user, directory, Path::new("/proc"))?
    } else {
        configure_cell_display_policy(android_command, request.user)?;
        false
    };
    timing.record("policy");
    dismiss_keyguard(android_command)?;
    wake_cell(android_command)?;
    timing.record("wake");
    let before = command_as_android_shell(android_command)
        .args(["activity", "stack", "list"])
        .output()?;
    let before = checked_activity_output(&before, "stack list before launch")?;
    let preexisting = matching_tasks_with(
        &before,
        BUILT_IN_DISPLAY,
        &request.package,
        Some(request.user),
        |_| true,
    )?
    .into_iter()
    .collect::<BTreeSet<_>>();
    timing.record("snapshot");
    let permission_dialog = resolve_permission_dialog(android_command, request.user)?;
    timing.record("permission_resolver");
    let launched_activity = start_activity(
        request,
        android_command,
        BUILT_IN_DISPLAY,
        permission_dialog.as_deref(),
    )?;
    timing.record("activity");
    let alias = if permission_dialog.as_deref() == Some(&launched_activity)
        || is_play_store_sign_in(&request.package, &launched_activity)
    {
        None
    } else {
        resolve_launch_alias(request, android_command, &launched_activity)?
    };
    timing.record("alias");

    let mut last_listing = String::new();
    for _ in 0..ACTIVITY_DISCOVERY_ATTEMPTS {
        let output = command_as_android_shell(android_command)
            .args(["activity", "stack", "list"])
            .output()?;
        last_listing = checked_activity_output(&output, "stack list")?;
        let tasks = discover_launch_candidates(
            &last_listing,
            BUILT_IN_DISPLAY,
            request,
            &launched_activity,
            alias.as_deref(),
            permission_dialog.as_deref(),
            &preexisting,
        )?;
        match tasks.as_slice() {
            [task] => {
                timing.record("discovery");
                let binding = control.register_direct(&request.package, *task, 0)?;
                timing.record("registration");
                return Ok(TaskBinding {
                    object: binding.object,
                    composer_display: binding.composer_display,
                    android_display: BUILT_IN_DISPLAY,
                    task: *task,
                });
            }
            [] => thread::sleep(DISCOVERY_INTERVAL),
            _ => {
                return Err(LaunchError::Discovery(format!(
                    "package {} created multiple tasks {tasks:?} on Android display {BUILT_IN_DISPLAY}",
                    request.package
                )));
            }
        }
    }
    Err(LaunchError::Discovery(format!(
        "package {} has no unambiguous completed activity task on Android display {BUILT_IN_DISPLAY}; preexisting tasks: {preexisting:?}; last activity listing: {}",
        request.package,
        bounded_text(&last_listing)
    )))
}

fn launch_candidates(completed: &[u64], preexisting: &BTreeSet<u64>) -> Vec<u64> {
    let created = completed
        .iter()
        .copied()
        .filter(|task| !preexisting.contains(task))
        .collect::<Vec<_>>();
    // Android may reuse a singleTask activity or restore its persisted task.
    // Registration is idempotent; requiring a new ID strands a healthy app.
    // Retain exact completed-activity matching and reject ambiguous choices.
    if created.is_empty() {
        completed.to_vec()
    } else {
        created
    }
}

struct LaunchTiming {
    started: Instant,
    previous: Instant,
    stages: Vec<(&'static str, u128)>,
    policy_cached: bool,
}

impl LaunchTiming {
    fn new() -> Self {
        let started = Instant::now();
        Self {
            started,
            previous: started,
            stages: Vec::new(),
            policy_cached: false,
        }
    }

    fn record(&mut self, stage: &'static str) {
        let now = Instant::now();
        self.stages
            .push((stage, now.duration_since(self.previous).as_micros()));
        self.previous = now;
    }

    fn finish(&self, ok: bool) {
        let stages = self
            .stages
            .iter()
            .map(|(stage, micros)| format!("{stage}_us={micros}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!(
            "droidloom-launch-timing ok={ok} policy_cached={} {stages} pending_us={} total_us={}",
            self.policy_cached,
            self.previous.elapsed().as_micros(),
            self.started.elapsed().as_micros(),
        );
    }
}

fn resolve_launch_alias(
    request: &LaunchRequest,
    android_command: &Path,
    completed: &str,
) -> Result<Option<String>, LaunchError> {
    // An explicit component that completed as itself cannot be a different
    // launcher alias. Keep PackageManager verification for aliases, implicit
    // intents, and trampoline successors, even when an old task also matches.
    if request
        .component
        .as_deref()
        .and_then(normalized_component)
        .as_deref()
        == Some(completed)
    {
        return Ok(None);
    }
    let mut command = command_as_android_shell(android_command);
    command.args([
        "package",
        "resolve-activity",
        "--user",
        &request.user.to_string(),
    ]);
    append_launch_intent(&mut command, request);
    let output = checked_activity_output(&command.output()?, "resolve launch alias")?;
    Ok(completed_alias(&output, &request.package, completed))
}

fn completed_alias(info: &str, package: &str, completed: &str) -> Option<String> {
    // ResolveInfo includes a nested ApplicationInfo with another name=. Read
    // only ActivityInfo, and accept an alias only if PackageManager explicitly
    // identifies it as the same activity that Android completed launching.
    let info = info
        .split_once("ActivityInfo:")?
        .1
        .split("ApplicationInfo:")
        .next()?;
    let field = |name| {
        info.split_whitespace()
            .find_map(|word| word.strip_prefix(name))
    };
    if field("packageName=")? != package {
        return None;
    }
    let alias = normalized_component(&format!("{package}/{}", field("name=")?))?;
    let target = normalized_component(&format!("{package}/{}", field("targetActivity=")?))?;
    (target == completed && alias != target).then_some(alias)
}

fn append_launch_intent(command: &mut Command, request: &LaunchRequest) {
    if let Some(component) = request.component.as_deref() {
        command.args(["-n", component]);
    } else {
        command.args([
            "-a",
            "android.intent.action.MAIN",
            "-c",
            "android.intent.category.LAUNCHER",
            "-p",
            &request.package,
        ]);
    }
}

fn configure_cell_display_policy(
    android_command: &Path,
    android_user: u32,
) -> Result<(), LaunchError> {
    // Keep HOME fullscreen but make ordinary built-in-display tasks use
    // Android's stock freeform mode. Denial owns their external placement;
    // the framework bridge updates only each task's private content bounds.
    let output = command_as_android_shell(android_command)
        .args(["window", "set-display-windowing-mode", "-d", "0", "5"])
        .output()?;
    checked_activity_output(&output, "enable built-in-display freeform tasks")?;

    // Legacy games may declare themselves non-resizable and otherwise stay
    // fullscreen on the private Android display without a host window.
    let output = command_as_android_shell(android_command)
        .args([
            "settings",
            "put",
            "global",
            "force_resizable_activities",
            "1",
        ])
        .output()?;
    checked_activity_output(&output, "enable legacy app window compatibility")?;

    // Denial owns the real user session and its lock boundary. Android's
    // keyguard has no physical panel to protect inside the cell and otherwise
    // imposes a separate ten-second WindowManager timeout while it is shown.
    let user = android_user.to_string();
    let output = command_as_android_shell(android_command)
        .args(["lock_settings", "set-disabled", "--user", &user, "true"])
        .output()?;
    checked_activity_output(&output, "disable cell lockscreen")?;

    // An Android cell has no physical panel whose screen timeout should blank
    // independently of its host windows. Persist the maximum supported timeout
    // for this Android user; unlike a shell-owned wake lock, it survives the
    // short-lived launcher process.
    let timeout = i32::MAX.to_string();
    let output = command_as_android_shell(android_command)
        .args([
            "settings",
            "--user",
            &user,
            "put",
            "system",
            "screen_off_timeout",
            &timeout,
        ])
        .output()?;
    checked_activity_output(&output, "disable cell screen timeout")?;

    // Android's framework default caps app rendering at 60 Hz even when HWC
    // exposes a single faster mode. A Droidloom display is paced by its host
    // output, so select Android's explicit "highest supported" setting rather
    // than baking a device-specific 90/120/240 Hz value into the cell image.
    // DisplayModeDirector maps positive infinity to the display's highest
    // advertised rate. Setting both bounds avoids a 60 Hz default render vote
    // making apps alternate between 60 Hz idle and host-rate touch boosts.
    for setting in ["peak_refresh_rate", "min_refresh_rate"] {
        let output = command_as_android_shell(android_command)
            .args([
                "settings", "--user", &user, "put", "system", setting, "Infinity",
            ])
            .output()?;
        checked_activity_output(&output, "match cell refresh rate to host")?;
    }
    Ok(())
}

fn wake_cell(android_command: &Path) -> Result<(), LaunchError> {
    // Wake the global power group and its adjacent displays. A display-scoped
    // request made while Android is globally asleep is not sufficient to make
    // the headless cell interactive on every framework version.
    let output = command_as_android_shell(android_command)
        .args(["power", "wakeup"])
        .output()?;
    checked_activity_output(&output, "power wakeup")?;
    Ok(())
}

fn dismiss_keyguard(android_command: &Path) -> Result<(), LaunchError> {
    let output = command_as_android_shell(android_command)
        .args(["window", "dismiss-keyguard"])
        .output()?;
    checked_activity_output(&output, "window dismiss-keyguard")?;
    Ok(())
}

fn start_activity(
    request: &LaunchRequest,
    android_command: &Path,
    android_display: u64,
    permission_dialog: Option<&str>,
) -> Result<String, LaunchError> {
    let display_argument = android_display.to_string();
    let user_argument = request.user.to_string();
    let mut last_error = None;
    for _ in 0..ACTIVITY_DISCOVERY_ATTEMPTS {
        let mut command = command_as_android_shell(android_command);
        command.args([
            "activity",
            "start-activity",
            // Android tracks the launch chain through no-display trampoline
            // activities. Do not export the first transient task we observe.
            "-W",
            "--display",
            &display_argument,
            "--windowingMode",
            "5",
            "--user",
            &user_argument,
        ]);
        append_launch_intent(&mut command, request);
        match checked_activity_output(&command.output()?, "start-activity") {
            Ok(output) => {
                return completed_launch_activity(&output, &request.package, permission_dialog);
            }
            Err(error) if is_pending_display_error(&error, android_display) => {
                last_error = Some(error);
                thread::sleep(DISCOVERY_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        LaunchError::Activity(format!(
            "Android display {android_display} did not become launchable"
        ))
    }))
}

fn resolve_permission_dialog(
    android_command: &Path,
    user: u32,
) -> Result<Option<String>, LaunchError> {
    // Ask PackageManager, rather than accepting a foreign package by name.
    // MATCH_SYSTEM_ONLY prevents an installed app from impersonating this UI.
    let output = command_as_android_shell(android_command)
        .args([
            "package",
            "resolve-activity",
            "--components",
            "--query-flags",
            "1048576",
            "--user",
            &user.to_string(),
            "-a",
            "android.content.pm.action.REQUEST_PERMISSIONS",
        ])
        .output()?;
    let output = checked_activity_output(&output, "resolve system permission dialog")?;
    let output = output.trim();
    if output == "No activity found" {
        return Ok(None);
    }
    normalized_component(output)
        .filter(|_| !output.chars().any(char::is_whitespace))
        .map(Some)
        .ok_or_else(|| {
            LaunchError::Activity(format!(
                "invalid system permission dialog resolution: {}",
                bounded_text(output)
            ))
        })
}

fn completed_launch_activity(
    output: &str,
    package: &str,
    permission_dialog: Option<&str>,
) -> Result<String, LaunchError> {
    if !output.lines().any(|line| line.trim() == "Status: ok") {
        return Err(LaunchError::Activity(format!(
            "Android did not complete the activity launch: {}",
            bounded_text(output)
        )));
    }
    let component = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Activity: "))
        .and_then(normalized_component)
        .ok_or_else(|| {
            LaunchError::Activity("launch result omitted its final activity".to_owned())
        })?;
    if component.split_once('/').map(|(owner, _)| owner) != Some(package)
        && permission_dialog != Some(component.as_str())
        && !is_play_store_sign_in(package, &component)
    {
        return Err(LaunchError::Activity(format!(
            "launch completed in {component}, outside requested package {package}"
        )));
    }
    Ok(component)
}

fn normalized_component(component: &str) -> Option<String> {
    let (package, activity) = component.split_once('/')?;
    if package.is_empty() || activity.is_empty() {
        return None;
    }
    Some(if activity.starts_with('.') {
        format!("{package}/{package}{activity}")
    } else {
        component.to_owned()
    })
}

// The optional Google package opens this authentication activity inside the
// Play Store's existing task. Keep this exception scoped to that exact pair;
// task ownership, user, display, visibility and ambiguity checks still apply.
fn is_play_store_sign_in(package: &str, component: &str) -> bool {
    package == "com.android.vending"
        && component == "com.google.android.gms/com.google.android.gms.auth.uiflows.minutemaid.MinuteMaidActivity"
}

fn is_pending_display_error(error: &LaunchError, android_display: u64) -> bool {
    let LaunchError::Activity(message) = error else {
        return false;
    };
    message.contains("Permission Denial:")
        && message.contains(&format!("with launchDisplayId={android_display}"))
}

fn command_as_android_shell(path: &Path) -> Command {
    #[cfg(target_os = "android")]
    {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new(path);
        // Android grants its documented `cmd`/`am` management surface to the
        // shell identity, not to arbitrary UID 0 Binder callers. Keep the
        // parent privileged for Droidloom's control socket and drop only each
        // short-lived framework command child to the standard shell UID/GID.
        command.uid(AID_SHELL).gid(AID_SHELL);
        command
    }
    #[cfg(not(target_os = "android"))]
    Command::new(path)
}

fn validate_request(request: &LaunchRequest) -> Result<(), LaunchError> {
    if request.package.is_empty()
        || !request
            .package
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        || !request.package.contains('.')
        || request.user > i32::MAX as u32
    {
        return Err(LaunchError::InvalidRequest(
            "package or Android user is outside the supported form".to_owned(),
        ));
    }
    if let Some(component) = request.component.as_deref() {
        let Some(activity) = component.strip_prefix(&request.package) else {
            return Err(LaunchError::InvalidRequest(
                "component must belong to the requested package".to_owned(),
            ));
        };
        let Some(activity) = activity.strip_prefix('/') else {
            return Err(LaunchError::InvalidRequest(
                "component must be a flattened PACKAGE/ACTIVITY name".to_owned(),
            ));
        };
        if activity.is_empty()
            || !activity
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'$'))
        {
            return Err(LaunchError::InvalidRequest(
                "component activity has invalid characters".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_display_density(dpi: u32) -> Result<(), LaunchError> {
    if dpi < MIN_DISPLAY_DPI || dpi > i32::MAX as u32 {
        return Err(LaunchError::InvalidRequest(format!(
            "display density must be between {MIN_DISPLAY_DPI} and {} dpi",
            i32::MAX
        )));
    }
    Ok(())
}

fn checked_activity_output(output: &Output, operation: &str) -> Result<String, LaunchError> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(LaunchError::Activity(format!(
            "{operation} exited {:?}: {}{}",
            output.status.code(),
            bounded_text(&stdout),
            bounded_text(&stderr)
        )));
    }
    if stdout.lines().any(|line| line.starts_with("Error:"))
        || stderr.lines().any(|line| line.starts_with("Error:"))
    {
        return Err(LaunchError::Activity(format!(
            "{operation}: {}{}",
            bounded_text(&stdout),
            bounded_text(&stderr)
        )));
    }
    Ok(stdout.into_owned())
}

fn bounded_text(text: &str) -> String {
    if text.len() <= MAX_DIAGNOSTIC_BYTES {
        return text.trim().to_owned();
    }
    let mut end = MAX_DIAGNOSTIC_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", text[..end].trim())
}

/// Parse `cmd activity stack list` and return matching leaf-task identities.
///
/// # Errors
///
/// Rejects numeric overflows in task records that otherwise match the target.
pub fn matching_tasks(
    listing: &str,
    target_display: u64,
    package: &str,
) -> Result<Vec<u64>, LaunchError> {
    matching_activity_tasks(listing, target_display, package, None)
}

fn matching_activity_tasks(
    listing: &str,
    target_display: u64,
    package: &str,
    activity: Option<&str>,
) -> Result<Vec<u64>, LaunchError> {
    matching_tasks_with(listing, target_display, package, None, |top| {
        activity.is_none() || top == activity
    })
}

fn matching_launch_tasks(
    listing: &str,
    display: u64,
    request: &LaunchRequest,
    completed: &str,
    alias: Option<&str>,
    permission_dialog: Option<&str>,
) -> Result<Vec<u64>, LaunchError> {
    if is_play_store_sign_in(&request.package, completed) {
        return matching_task_records(
            listing,
            display,
            &request.package,
            Some(request.user),
            true,
            |top| top.is_some_and(|top| {
                is_play_store_sign_in(&request.package, top)
                    || top.split_once('/').map(|(owner, _)| owner) == Some(request.package.as_str())
            }),
        );
    }
    matching_tasks_with(
        listing,
        display,
        &request.package,
        Some(request.user),
        |top| {
            let Some(top) = top else {
                return false;
            };
            top == completed || Some(top) == alias
            // Permission UI can appear after start-activity -W returns.
            || Some(top) == permission_dialog
            // Or disappear before task discovery, returning to the app.
            || (Some(completed) == permission_dialog
                && top.split_once('/').map(|(owner, _)| owner) == Some(request.package.as_str()))
        },
    )
}

fn discover_launch_candidates(
    listing: &str,
    display: u64,
    request: &LaunchRequest,
    completed: &str,
    alias: Option<&str>,
    permission_dialog: Option<&str>,
    preexisting: &BTreeSet<u64>,
) -> Result<Vec<u64>, LaunchError> {
    let exact = matching_launch_tasks(
        listing,
        display,
        request,
        completed,
        alias,
        permission_dialog,
    )?;
    if !exact.is_empty() {
        return Ok(launch_candidates(&exact, preexisting));
    }
    // -W can complete the launcher before an app replaces it with its game
    // or onboarding activity. Follow a visible successor only inside the
    // requested package's task, user and display. Play Store may instead hand
    // off to its specific Google sign-in activity. Other foreign activities
    // and hidden background tasks remain ineligible.
    if completed.split_once('/').map(|(owner, _)| owner) != Some(request.package.as_str()) {
        return Ok(Vec::new());
    }
    let successors = matching_task_records(
        listing,
        display,
        &request.package,
        Some(request.user),
        true,
        |top| {
            top.is_some_and(|top| {
                top.split_once('/').map(|(owner, _)| owner) == Some(request.package.as_str())
                    || is_play_store_sign_in(&request.package, top)
            })
        },
    )?;
    // Prefer a task created by this launch. A unique restored task can also
    // complete a handoff; multiple eligible tasks remain an error upstream.
    Ok(launch_candidates(&successors, preexisting))
}

fn matching_tasks_with(
    listing: &str,
    target_display: u64,
    package: &str,
    user: Option<u32>,
    accept_activity: impl FnMut(Option<&str>) -> bool,
) -> Result<Vec<u64>, LaunchError> {
    matching_task_records(
        listing,
        target_display,
        package,
        user,
        false,
        accept_activity,
    )
}

fn matching_task_records(
    listing: &str,
    target_display: u64,
    package: &str,
    user: Option<u32>,
    require_visible: bool,
    mut accept_activity: impl FnMut(Option<&str>) -> bool,
) -> Result<Vec<u64>, LaunchError> {
    let mut display = None;
    let mut tasks = BTreeSet::new();
    for line in listing.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("RootTask id=") {
            display = numeric_field(trimmed, "displayId=");
            continue;
        }
        if display != Some(target_display) || !trimmed.starts_with("taskId=") {
            continue;
        }
        let Some((identity, remainder)) = trimmed["taskId=".len()..].split_once(':') else {
            continue;
        };
        if require_visible
            && !remainder
                .split_whitespace()
                .any(|field| field == "visible=true")
        {
            continue;
        }
        let task_name = remainder.split_whitespace().next().unwrap_or("");
        let package_match = task_name == package
            || task_name
                .strip_prefix(package)
                .is_some_and(|suffix| suffix.starts_with('/'));
        if package_match
            && user.is_none_or(|user| numeric_field(remainder, "userId=") == Some(u64::from(user)))
        {
            let top = remainder
                .split_whitespace()
                .find_map(|field| field.strip_prefix("topActivity=ComponentInfo{"))
                .and_then(|component| component.strip_suffix('}'))
                .and_then(normalized_component);
            if !accept_activity(top.as_deref()) {
                continue;
            }
            let identity = identity.parse::<u64>().map_err(|_| {
                LaunchError::Discovery("matching Android task ID overflowed u64".to_owned())
            })?;
            if identity == 0 {
                return Err(LaunchError::Discovery(
                    "Android reported task identity zero".to_owned(),
                ));
            }
            tasks.insert(identity);
        }
    }
    Ok(tasks.into_iter().collect())
}

fn numeric_field(line: &str, field: &str) -> Option<u64> {
    let value = line
        .split_whitespace()
        .find_map(|token| token.strip_prefix(field))?;
    value.parse().ok()
}

struct TaskControlClient {
    socket: SeqPacket,
    next_request: u64,
}

impl TaskControlClient {
    fn connect(path: &Path) -> Result<Self, LaunchError> {
        let socket = SeqPacket::connect(path)?;
        let client = Self {
            socket,
            next_request: 1,
        };
        client.send(&ControlRequest::Hello {
            min_major: PROTOCOL_MAJOR,
            max_major: PROTOCOL_MAJOR,
        })?;
        match client.receive()? {
            ControlResponse::ServerHello { major, .. } if major == PROTOCOL_MAJOR => Ok(client),
            response => Err(LaunchError::Control(format!(
                "unexpected hello response {response:?}"
            ))),
        }
    }

    fn register_direct(
        &mut self,
        package: &str,
        task: u64,
        android_display: u32,
    ) -> Result<TaskBinding, LaunchError> {
        let request_id = self.request_id()?;
        self.send(&ControlRequest::RegisterDirect {
            request_id,
            package: package.to_owned(),
            task,
            android_display,
        })?;
        match self.receive()? {
            ControlResponse::Bound {
                request_id: actual,
                object,
                task: actual_task,
                display,
                android_display: actual_android_display,
            } if actual == request_id
                && actual_task == task
                && actual_android_display == android_display =>
            {
                Ok(TaskBinding {
                    object,
                    composer_display: display,
                    android_display: u64::from(android_display),
                    task,
                })
            }
            response => Err(response_error(request_id, response)),
        }
    }

    fn request_id(&mut self) -> Result<u64, LaunchError> {
        let identity = self.next_request;
        self.next_request = self
            .next_request
            .checked_add(1)
            .ok_or_else(|| LaunchError::Control("request identity exhausted".to_owned()))?;
        Ok(identity)
    }

    fn send(&self, request: &ControlRequest) -> Result<(), LaunchError> {
        let record = encode_request(request)?;
        self.socket.send_record(&record, &[])?;
        Ok(())
    }

    fn receive(&self) -> Result<ControlResponse, LaunchError> {
        let record = self.socket.receive_record()?;
        if !record.descriptors.is_empty() {
            return Err(LaunchError::Control(
                "task-control response carried forbidden descriptors".to_owned(),
            ));
        }
        Ok(decode_response(&record.bytes)?)
    }
}

fn response_error(request_id: u64, response: ControlResponse) -> LaunchError {
    match response {
        ControlResponse::Error {
            request_id: actual,
            code,
            message,
        } if actual == request_id => LaunchError::Control(format!("{code:?}: {message}")),
        response => LaunchError::Control(format!(
            "request {request_id} received unexpected response {response:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_completed_component_needs_no_package_manager_command() {
        let request = LaunchRequest {
            package: "org.example.notes".to_owned(),
            component: Some("org.example.notes/.MainActivity".to_owned()),
            user: 0,
        };
        // A nonexistent command also proves the shortcut spawns no process.
        let missing = Path::new("/nonexistent/droidloom-test-cmd");
        assert_eq!(
            resolve_launch_alias(
                &request,
                missing,
                "org.example.notes/org.example.notes.MainActivity"
            )
            .unwrap(),
            None
        );
        assert!(
            resolve_launch_alias(
                &request,
                missing,
                "org.example.notes/org.example.notes.RealActivity"
            )
            .is_err()
        );
        assert!(
            resolve_launch_alias(
                &LaunchRequest {
                    component: None,
                    ..request
                },
                missing,
                "org.example.notes/org.example.notes.MainActivity"
            )
            .is_err()
        );
    }

    #[test]
    fn package_manager_alias_matches_completed_real_activity() {
        for (package, alias, target) in [
            (
                "com.android.settings",
                "com.android.settings.Settings",
                "com.android.settings.homepage.SettingsHomepageActivity",
            ),
            (
                "org.mozilla.firefox",
                "org.mozilla.firefox.App",
                "org.mozilla.fenix.HomeActivity",
            ),
        ] {
            let info = format!(
                "ActivityInfo:\n name={alias}\n packageName={package}\n taskAffinity={package} targetActivity={target}\n ApplicationInfo:\n name=OtherApplication"
            );
            let completed = format!("{package}/{target}");
            assert_eq!(
                completed_alias(&info, package, &completed),
                Some(format!("{package}/{alias}"))
            );
            assert_eq!(
                completed_alias(&info, package, &format!("{package}/TrampolineSuccessor")),
                None
            );
            assert_eq!(completed_alias(&info, "foreign.package", &completed), None);
        }
        assert_eq!(
            completed_alias(
                "ActivityInfo:\n name=LauncherActivity packageName=org.example.files targetActivity=null",
                "org.example.files",
                "org.example.files/FilesActivity"
            ),
            None
        );
    }

    fn handoff_task(id: u64, top: &str, visible: bool, user: u32, display: u64) -> String {
        format!(
            "RootTask id={id} displayId={display} userId={user}\n  taskId={id}: com.halfbrick.fruitninjafree/.Game userId={user} visible={visible} topActivity=ComponentInfo{{{top}}}\n"
        )
    }

    fn discover_fruit(listing: &str, preexisting: &[u64]) -> Vec<u64> {
        let request = LaunchRequest {
            package: "com.halfbrick.fruitninjafree".into(),
            component: Some("com.halfbrick.fruitninjafree/.Launcher".into()),
            user: 0,
        };
        discover_launch_candidates(
            listing,
            0,
            &request,
            "com.halfbrick.fruitninjafree/com.halfbrick.fruitninjafree.Launcher",
            None,
            None,
            &preexisting.iter().copied().collect(),
        )
        .unwrap()
    }

    #[test]
    fn follows_game_handoff_after_launcher_completion_and_on_reopen() {
        let game = handoff_task(7, "com.halfbrick.fruitninjafree/.Game", true, 0, 0);
        assert_eq!(discover_fruit(&game, &[]), vec![7]);
        assert_eq!(discover_fruit(&game, &[7]), vec![7]);
        let age = handoff_task(7, "com.halfbrick.fruitninjafree/.AgeScreen", true, 0, 0);
        assert_eq!(discover_fruit(&age, &[]), vec![7]);
    }

    #[test]
    fn handoff_requires_visible_owned_task_user_and_display() {
        let game = "com.halfbrick.fruitninjafree/.Game";
        for listing in [
            handoff_task(7, game, false, 0, 0),
            handoff_task(7, game, true, 10, 0),
            handoff_task(7, game, true, 0, 2),
            handoff_task(7, "com.halfbrick.fruitninjafree.extra/.Game", true, 0, 0),
            handoff_task(7, "org.example.foreign/.Main", true, 0, 0),
            handoff_task(7, game, true, 0, 0).replace("userId=0", ""),
            handoff_task(7, game, true, 0, 0).replace("visible=true", ""),
            handoff_task(7, game, true, 0, 0).replace(
                "taskId=7: com.halfbrick.fruitninjafree/",
                "taskId=7: org.example.foreign/",
            ),
        ] {
            assert!(discover_fruit(&listing, &[]).is_empty(), "{listing}");
        }
    }

    #[test]
    fn handoff_prefers_new_tasks_but_does_not_guess_between_ambiguous_tasks() {
        let game = "com.halfbrick.fruitninjafree/.Game";
        let listing = handoff_task(7, game, true, 0, 0) + &handoff_task(8, game, true, 0, 0);
        assert_eq!(discover_fruit(&listing, &[7]), vec![8]);
        assert_eq!(discover_fruit(&listing, &[]), vec![7, 8]);
        assert_eq!(discover_fruit(&listing, &[7, 8]), vec![7, 8]);
    }

    #[test]
    fn exact_completed_activity_takes_precedence_over_handoff_fallback() {
        let listing = handoff_task(7, "com.halfbrick.fruitninjafree/.Launcher", true, 0, 0)
            + &handoff_task(8, "com.halfbrick.fruitninjafree/.Game", true, 0, 0);
        assert_eq!(discover_fruit(&listing, &[]), vec![7]);
    }

    #[test]
    fn reopening_accepts_a_unique_restored_or_already_running_task() {
        assert_eq!(launch_candidates(&[8], &BTreeSet::from([8])), vec![8]);
        assert_eq!(launch_candidates(&[8, 11], &BTreeSet::from([8])), vec![11]);
        assert_eq!(
            launch_candidates(&[8, 11], &BTreeSet::from([8, 11])),
            vec![8, 11]
        );
        assert!(launch_candidates(&[], &BTreeSet::from([8])).is_empty());
    }

    const STACKS: &str = r"RootTask id=1 bounds=[0,0][1080,2400] displayId=0 userId=0
 configuration={}
  taskId=8: com.android.launcher3/.Launcher bounds=[0,0][1080,2400] userId=0 visible=true
RootTask id=4 bounds=[0,0][900,1600] displayId=41 userId=0
 configuration={}
  taskId=27: org.example.notes/.MainActivity bounds=[0,0][900,1600] userId=0 visible=true
  taskId=28: org.example.notesplus/.MainActivity bounds=[0,0][900,1600] userId=0 visible=true
RootTask id=5 bounds=[0,0][900,1600] displayId=42 userId=0
 configuration={}
  taskId=29: org.example.notes/.SecondActivity bounds=[0,0][900,1600] userId=0 visible=true
";

    #[test]
    fn follows_androids_completed_trampoline_launch() {
        let activity = completed_launch_activity(
            "Starting: Intent { cmp=org.example.files/.LauncherActivity }\nStatus: ok\nActivity: org.example.files/.FilesActivity\nComplete\n",
            "org.example.files",
            None,
        ).unwrap();
        assert_eq!(
            activity,
            "org.example.files/org.example.files.FilesActivity"
        );
        let listing = "RootTask id=137 displayId=0 userId=0\n  taskId=137: org.example.files/.LauncherActivity topActivity=ComponentInfo{org.example.files/.LauncherActivity}\nRootTask id=138 displayId=0 userId=0\n  taskId=138: org.example.files/.FilesActivity topActivity=ComponentInfo{org.example.files/org.example.files.FilesActivity}\n";
        assert_eq!(
            matching_activity_tasks(listing, 0, "org.example.files", Some(&activity)).unwrap(),
            vec![138]
        );
        assert!(
            matching_activity_tasks(listing, 1, "org.example.files", Some(&activity))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_incomplete_or_foreign_launch_results() {
        for output in [
            "Status: timeout\nActivity: org.example.files/.FilesActivity\n",
            "Status: ok\n",
            "Status: ok\nActivity: org.example.other/.FilesActivity\n",
            "Status: ok\nActivity: malformed\n",
        ] {
            assert!(completed_launch_activity(output, "org.example.files", None).is_err());
        }
    }

    const PERMISSION_DIALOG: &str = "com.android.permissioncontroller/com.android.permissioncontroller.permission.ui.GrantPermissionsActivity";

    #[test]
    fn play_store_sign_in_stays_in_its_visible_owned_task() {
        let store = "com.android.vending/com.android.vending.AssetBrowserActivity";
        let auth = "com.google.android.gms/com.google.android.gms.auth.uiflows.minutemaid.MinuteMaidActivity";
        let request = LaunchRequest { package: "com.android.vending".into(), component: None, user: 0 };
        let task = |id, owner, top, visible, display, user| format!(
            "RootTask id={id} displayId={display} userId={user}\n  taskId={id}: {owner}/.Main userId={user} visible={visible} topActivity=ComponentInfo{{{top}}}\n"
        );
        let discover = |listing: &str, completed: &str| discover_launch_candidates(
            listing, 0, &request, completed, None, None, &BTreeSet::from([9])
        ).unwrap();
        let own = task(9, "com.android.vending", auth, true, 0, 0);
        assert_eq!(discover(&own, store), vec![9]);
        assert_eq!(discover(&own, auth), vec![9]);
        assert_eq!(discover(&task(9, "com.android.vending", store, true, 0, 0), auth), vec![9]);
        for listing in [
            task(9, "com.google.android.gms", auth, true, 0, 0),
            task(9, "com.android.vending.extra", auth, true, 0, 0),
            task(9, "com.android.vending", auth, false, 0, 0),
            task(9, "com.android.vending", auth, true, 1, 0),
            task(9, "com.android.vending", auth, true, 0, 10),
            task(9, "com.android.vending", "com.google.android.gms/.OtherActivity", true, 0, 0),
            own.replace("userId=0", ""),
        ] {
            assert!(discover(&listing, store).is_empty(), "{listing}");
            assert!(discover(&listing, auth).is_empty(), "{listing}");
        }
        let ambiguous = task(10, "com.android.vending", auth, true, 0, 0)
            + &task(11, "com.android.vending", auth, true, 0, 0);
        assert_eq!(discover(&ambiguous, store), vec![10, 11]);
        let output = format!("Status: ok\nActivity: {auth}\nComplete\n");
        assert_eq!(completed_launch_activity(&output, "com.android.vending", None).unwrap(), auth);
        assert!(completed_launch_activity(&output, "org.example.other", None).is_err());
    }

    fn contacts_request() -> LaunchRequest {
        LaunchRequest {
            package: "com.google.android.contacts".into(),
            component: None,
            user: 0,
        }
    }

    #[test]
    fn permission_completion_requires_the_framework_resolved_component() {
        let output = format!("Status: ok\nActivity: {PERMISSION_DIALOG}\nComplete\n");
        assert_eq!(
            completed_launch_activity(
                &output,
                "com.google.android.contacts",
                Some(PERMISSION_DIALOG)
            )
            .unwrap(),
            PERMISSION_DIALOG
        );
        assert!(completed_launch_activity(&output, "com.google.android.contacts", None).is_err());
        for component in [
            "com.android.permissioncontroller/.SomeOtherActivity",
            "org.example.imposter/com.android.permissioncontroller.permission.ui.GrantPermissionsActivity",
        ] {
            assert!(
                completed_launch_activity(
                    &format!("Status: ok\nActivity: {component}\n"),
                    "com.google.android.contacts",
                    Some(PERMISSION_DIALOG)
                )
                .is_err()
            );
        }
    }

    #[test]
    fn permission_dialog_is_exported_only_inside_the_owned_task_and_user() {
        let task = |id, package, user| {
            format!(
                "RootTask id={id} displayId=0 userId={user}\n  taskId={id}: {package}/.PeopleActivity userId={user} topActivity=ComponentInfo{{{PERMISSION_DIALOG}}}\n"
            )
        };
        let listing = [
            task(10, "com.google.android.contacts", 0),
            task(11, "com.android.contacts", 0),
            task(12, "com.google.android.contacts", 10),
            task(13, "com.android.permissioncontroller", 0),
            task(14, "com.google.android.contacts.extra", 0),
        ]
        .concat();
        assert_eq!(
            matching_launch_tasks(
                &listing,
                0,
                &contacts_request(),
                PERMISSION_DIALOG,
                None,
                Some(PERMISSION_DIALOG)
            )
            .unwrap(),
            vec![10]
        );
        assert!(
            matching_launch_tasks(
                &listing,
                1,
                &contacts_request(),
                PERMISSION_DIALOG,
                None,
                Some(PERMISSION_DIALOG)
            )
            .unwrap()
            .is_empty()
        );
        let missing_user = task(10, "com.google.android.contacts", 0).replace("userId=0", "");
        assert!(
            matching_launch_tasks(
                &missing_user,
                0,
                &contacts_request(),
                PERMISSION_DIALOG,
                None,
                Some(PERMISSION_DIALOG)
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn permission_dialog_can_appear_or_close_during_task_discovery() {
        let activity = "com.google.android.contacts/com.android.contacts.activities.PeopleActivity";
        let task = |top| {
            format!(
                "RootTask id=10 displayId=0 userId=0\n  taskId=10: {activity} userId=0 topActivity=ComponentInfo{{{top}}}\n"
            )
        };
        assert_eq!(
            matching_launch_tasks(
                &task(PERMISSION_DIALOG),
                0,
                &contacts_request(),
                activity,
                None,
                Some(PERMISSION_DIALOG)
            )
            .unwrap(),
            vec![10]
        );
        assert_eq!(
            matching_launch_tasks(
                &task(activity),
                0,
                &contacts_request(),
                PERMISSION_DIALOG,
                None,
                Some(PERMISSION_DIALOG)
            )
            .unwrap(),
            vec![10]
        );
        assert!(
            matching_launch_tasks(
                &task("org.example.foreign/.Activity"),
                0,
                &contacts_request(),
                PERMISSION_DIALOG,
                None,
                Some(PERMISSION_DIALOG)
            )
            .unwrap()
            .is_empty()
        );
        // The permission path must not relax ordinary trampoline matching.
        assert!(
            matching_launch_tasks(
                &task("com.google.android.contacts/.Trampoline"),
                0,
                &contacts_request(),
                activity,
                None,
                Some(PERMISSION_DIALOG)
            )
            .unwrap()
            .is_empty()
        );
        let ambiguous = format!(
            "{}{}",
            task(PERMISSION_DIALOG),
            task(PERMISSION_DIALOG)
                .replace("id=10", "id=20")
                .replace("taskId=10", "taskId=20")
        );
        let candidates = matching_launch_tasks(
            &ambiguous,
            0,
            &contacts_request(),
            PERMISSION_DIALOG,
            None,
            Some(PERMISSION_DIALOG),
        )
        .unwrap();
        assert_eq!(
            launch_candidates(&candidates, &BTreeSet::new()),
            vec![10, 20]
        );
    }

    #[test]
    fn completed_activity_can_differ_from_task_base_alias() {
        let listing = "RootTask id=12 displayId=0 userId=0\n  taskId=12: org.example.settings/.Settings topActivity=ComponentInfo{org.example.settings/.Homepage}\n";
        assert_eq!(
            matching_activity_tasks(
                listing,
                0,
                "org.example.settings",
                Some("org.example.settings/org.example.settings.Homepage")
            )
            .unwrap(),
            vec![12]
        );
    }

    #[test]
    fn selects_exact_package_on_exact_display() {
        assert_eq!(
            matching_tasks(STACKS, 41, "org.example.notes").unwrap(),
            vec![27]
        );
        assert_eq!(
            matching_tasks(STACKS, 42, "org.example.notes").unwrap(),
            vec![29]
        );
    }

    #[test]
    fn package_prefix_does_not_match() {
        assert!(
            matching_tasks(STACKS, 41, "org.example.note")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn retries_only_the_target_display_permission_race() {
        let pending = LaunchError::Activity(
            "Permission Denial: starting Settings with launchDisplayId=7".to_owned(),
        );
        assert!(is_pending_display_error(&pending, 7));
        assert!(!is_pending_display_error(&pending, 8));
        assert!(!is_pending_display_error(
            &LaunchError::Activity("Error: Activity not found".to_owned()),
            7
        ));
    }

    #[test]
    fn display_density_uses_androids_supported_numeric_range() {
        assert!(validate_display_density(72).is_ok());
        assert!(validate_display_density(175).is_ok());
        assert!(validate_display_density(71).is_err());
        assert!(validate_display_density(u32::MAX).is_err());
    }

    #[test]
    fn validates_component_ownership() {
        let request = LaunchRequest {
            package: "org.example.notes".to_owned(),
            component: Some("org.example.other/.Main".to_owned()),
            user: 0,
        };
        assert!(matches!(
            validate_request(&request),
            Err(LaunchError::InvalidRequest(_))
        ));
    }
}
