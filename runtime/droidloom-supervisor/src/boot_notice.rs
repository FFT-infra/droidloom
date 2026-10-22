//! The desktop notification that covers Android's boot for the session.
//!
//! The unit's `droidloomctl start --cell` shows it and starts the cell; the
//! recovery companion follows the boot and closes it once Android is ready.
//! Either replaces it with the reason Android did not start. The notice's ID
//! lives in the session's runtime directory, so a restart reuses the notice.
use std::{
    env, fs,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    process::Stdio,
};

const SERVICE: &str = "org.freedesktop.Notifications";
const OBJECT: &str = "/org/freedesktop/Notifications";
/// Past a start and Android's readiness wait: a boot stopped before it was
/// followed to the end leaves the notice no longer than that.
const STARTING_TIMEOUT_MS: u32 = 160_000;

/// The session's boot notice; ID 0 when there is none.
pub struct BootNotice {
    id: u32,
    record: Option<PathBuf>,
}

impl BootNotice {
    /// No notice: boot is followed without telling the desktop.
    #[must_use]
    pub fn none() -> Self {
        Self { id: 0, record: None }
    }

    /// The notice a start showed, if it is still up.
    #[must_use]
    pub fn current() -> Self {
        let record = record();
        let id = record
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|id| id.trim().parse().ok())
            .unwrap_or(0);
        Self { id, record }
    }

    /// Shows that Droidloom is starting, in place of an earlier notice.
    #[must_use]
    pub fn starting() -> Self {
        let mut notice = Self::current();
        notice.show(
            "Droidloom is starting",
            "Android apps open once it is ready.",
            STARTING_TIMEOUT_MS,
        );
        notice
    }

    /// Android is ready: the notice goes.
    pub fn ready(self) {
        if self.id == 0 {
            return;
        }
        let _ = busctl(&["CloseNotification", "u", &self.id.to_string()]);
        if let Some(path) = &self.record {
            let _ = fs::remove_file(path);
        }
    }

    /// Android did not start: the notice says why and stays until dismissed.
    pub fn failed(mut self, reason: &str) {
        if self.id == 0 {
            return;
        }
        let reason = reason.lines().next().unwrap_or(reason);
        self.show("Droidloom could not start", reason, 0);
    }

    fn show(&mut self, summary: &str, body: &str, timeout_ms: u32) {
        let replaces = self.id.to_string();
        let timeout = timeout_ms.to_string();
        let shown = busctl(&[
            "Notify",
            "susssasa{sv}i",
            "Droidloom",
            &replaces,
            "",
            summary,
            body,
            "0",
            "0",
            &timeout,
        ]);
        // "u 42": the ID of the notice shown.
        let Some(id) = shown.and_then(|reply| reply.trim().strip_prefix("u ")?.parse().ok()) else {
            return;
        };
        self.id = id;
        if let Some(path) = &self.record {
            if let Some(directory) = path.parent() {
                let _ = fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(directory);
            }
            let _ = fs::write(path, format!("{id}\n"));
        }
    }
}

fn record() -> Option<PathBuf> {
    let runtime = PathBuf::from(env::var_os("XDG_RUNTIME_DIR")?);
    runtime
        .is_absolute()
        .then(|| runtime.join("droidloom/boot-notice"))
}

/// Calls the session's notification service; None without one.
fn busctl(call: &[&str]) -> Option<String> {
    let output = droidloom_cpu_placement::command("busctl")
        .args(["--user", "call", SERVICE, OBJECT, SERVICE])
        .args(call)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}
