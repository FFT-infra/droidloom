//! Verify complete service orchestration and the internal recovery boundary.
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

use droidloom_supervisor::session_binding::{SessionRecord, parse_record};
use droidloom_window_policy::SessionMode;

#[test]
fn start_waits_for_boot_and_keeps_progress_out_of_json() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    for (json, ready) in [(false, true), (false, false), (true, true), (true, false)] {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let worker = std::thread::spawn(move || {
            for expected in ["start", "wait_ready"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes).unwrap();
                let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(request["request"]["command"], expected);
                let (ok, message) = if expected == "start" {
                    (true, "started cell u1000")
                } else {
                    stream.write_all(b"{\"progress\":\"Android boot phase: waiting for Android to complete boot\"}\n").unwrap();
                    if ready {
                        (true, "Droidloom is running; Android is ready")
                    } else {
                        (false, "Android looks stuck. Check the logs with: journalctl -b -u droidloomd.service")
                    }
                };
                let mut response = serde_json::to_vec(
                    &serde_json::json!({"ok": ok, "state": "running", "message": message}),
                ).unwrap();
                response.push(b'\n');
                stream.write_all(&response).unwrap();
            }
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_droidloomctl"));
        command.args(["--socket"]).arg(socket).arg("start");
        if json { command.arg("--json"); }
        let output = command.output().unwrap();
        worker.join().unwrap();
        assert_eq!(output.status.success(), ready);
        let stderr = String::from_utf8(output.stderr).unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stderr.contains("Android boot phase:"), !json);
        assert!(!stdout.contains("Android boot phase:"));
        if json {
            let response: serde_json::Value = serde_json::from_str(&stdout).unwrap();
            assert_eq!(response["ok"], ready);
        } else if ready {
            assert!(stdout.contains("Android is ready"));
        } else {
            assert!(stdout.is_empty(), "failed boot must not print ready success");
            assert!(stderr.contains("Android looks stuck"));
        }
    }
}

struct SessionFixture {
    directory: tempfile::TempDir,
    _wayland: std::os::unix::net::UnixListener,
}

impl SessionFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let systemctl = directory.path().join("systemctl");
        fs::write(&systemctl, r#"#!/bin/sh
printf '%s\n' "$*" >> "$CALL_LOG"
case "$*" in
  '--user show --property=ActiveState --value droidloom.service')
    printf '%s\n' "${SERVICE_STATE:-inactive}"
    ;;
esac
"#).unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
        let wayland = std::os::unix::net::UnixListener::bind(directory.path().join("wayland-test")).unwrap();
        Self { directory, _wayland: wayland }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_droidloomctl"));
        command.args(args)
            .env("PATH", self.directory.path())
            .env("CALL_LOG", self.directory.path().join("calls"))
            .env("XDG_RUNTIME_DIR", self.directory.path())
            .env("WAYLAND_DISPLAY", "wayland-test")
            .env("SERVICE_STATE", "inactive")
            .env_remove("DROIDLOOM_HOST_NAVIGATION")
            .env_remove("XDG_SESSION_ID")
            .env_remove("DISPLAY")
            .env_remove("XDG_CURRENT_DESKTOP");
        command
    }

    fn record_path(&self) -> std::path::PathBuf { self.directory.path().join("droidloom/session.env") }
    fn calls(&self) -> String { fs::read_to_string(self.directory.path().join("calls")).unwrap_or_default() }
    fn clear_calls(&self) { fs::write(self.directory.path().join("calls"), "").unwrap(); }
    fn mode(&self) -> SessionMode { parse_record(&fs::read_to_string(self.record_path()).unwrap()).unwrap().mode() }

    fn hold_lock(&self, name: &str) -> fs::File {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let runtime = self.directory.path().join("droidloom");
        if !runtime.exists() { fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap(); }
        let file = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .mode(0o600).open(runtime.join(name)).unwrap();
        // SAFETY: the file is owned by this test and retained until the simulated owner stops.
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        file
    }
}

#[test]
fn lifecycle_controls_the_service_without_importing_global_environment() {
    for operation in ["start", "stop", "restart"] {
        let fixture = SessionFixture::new();
        let mut command = fixture.command(&[operation, "--json"]);
        if operation != "stop" { command.arg("--no-wait"); }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let expected = if operation == "stop" { "stop" } else { "start" };
        assert!(fixture.calls().contains(&format!("--user {expected} droidloom.service")));
        assert!(!fixture.calls().contains("import-environment"));
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["state"], if operation == "stop" { "stopped" } else { "running" });
        if operation != "stop" {
            let record = parse_record(&fs::read_to_string(fixture.record_path()).unwrap()).unwrap();
            let SessionRecord::Bound(binding) = record else { panic!("new starts must be bound") };
            assert_eq!(binding.compositor.pid, std::process::id());
            assert!(binding.compositor.socket.is_absolute());
            assert!(!binding.host_navigation);
        }
    }
}

#[test]
fn explicit_mode_reaches_service_and_restart_preserves_it() {
    let fixture = SessionFixture::new();
    let run = |args: &[&str]| fixture.command(args).arg("--no-wait").output().unwrap();
    assert!(run(&["start", "--mode", "mobile"]).status.success());
    assert_eq!(fixture.mode(), SessionMode::Mobile);
    fs::set_permissions(fixture.record_path().parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(run(&["restart"]).status.success());
    assert_eq!(fixture.mode(), SessionMode::Mobile);
    assert_eq!(fs::metadata(fixture.record_path().parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(run(&["start"]).status.success());
    assert_eq!(fixture.mode(), SessionMode::Desktop);
    assert!(!run(&["start", "--mode", "tablet"]).status.success());
}

#[test]
fn same_session_start_is_idempotent_while_owner_is_alive() {
    let fixture = SessionFixture::new();
    assert!(fixture.command(&["start", "--no-wait"]).output().unwrap().status.success());
    let _lease = fixture.hold_lock("owner.lock");
    let before = fs::read(fixture.record_path()).unwrap();
    fixture.clear_calls();
    let output = fixture.command(&["start", "--no-wait"]).env("SERVICE_STATE", "active").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!fixture.calls().contains("--user start "));
    assert!(!fixture.calls().contains("--user stop "));
    assert_eq!(fs::read(fixture.record_path()).unwrap(), before);
}

#[test]
fn foreign_owner_is_rejected_before_environment_or_service_mutation() {
    let fixture = SessionFixture::new();
    let _other = std::os::unix::net::UnixListener::bind(fixture.directory.path().join("wayland-other")).unwrap();
    let started = fixture.command(&["start", "--no-wait"])
        .env("WAYLAND_DISPLAY", "wayland-other").output().unwrap();
    assert!(started.status.success(), "{started:?}");
    let _lease = fixture.hold_lock("owner.lock");
    let before = fs::read(fixture.record_path()).unwrap();
    fixture.clear_calls();
    let output = fixture.command(&["start", "--mode", "mobile", "--no-wait"])
        .env("SERVICE_STATE", "active").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("another graphical session"));
    assert_eq!(fs::read(fixture.record_path()).unwrap(), before);
    let calls = fixture.calls();
    for forbidden in ["--user stop ", "--user start ", "daemon-reload", "import-environment"] {
        assert!(!calls.contains(forbidden), "{calls}");
    }
}

#[test]
fn legacy_active_service_is_not_a_stale_endpoint() {
    let fixture = SessionFixture::new();
    fs::create_dir(fixture.record_path().parent().unwrap()).unwrap();
    fs::set_permissions(fixture.record_path().parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(fixture.record_path(), "DROIDLOOM_MODE=mobile\n").unwrap();
    let output = fixture.command(&["start", "--no-wait"]).env("SERVICE_STATE", "active").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("legacy or starting"));
    assert_eq!(fs::read_to_string(fixture.record_path()).unwrap(), "DROIDLOOM_MODE=mobile\n");
    assert!(!fixture.calls().contains("--user start "));
    assert!(!fixture.calls().contains("--user stop "));
}

#[test]
fn a_busy_command_lock_does_not_run_systemctl_or_write_binding() {
    let fixture = SessionFixture::new();
    let _lock = fixture.hold_lock("command.lock");
    let output = fixture.command(&["start", "--no-wait"]).output().unwrap();
    assert!(!output.status.success());
    assert!(!fixture.record_path().exists());
    assert!(fixture.calls().is_empty());
}

#[test]
fn a_failed_stop_does_not_publish_the_new_mode() {
    let fixture = SessionFixture::new();
    assert!(fixture.command(&["start", "--no-wait"]).output().unwrap().status.success());
    let _lease = fixture.hold_lock("owner.lock");
    let before = fs::read(fixture.record_path()).unwrap();
    fixture.clear_calls();
    let output = fixture.command(&["restart", "--mode", "mobile", "--no-wait"])
        .env("SERVICE_STATE", "active").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("lease remained held"));
    assert!(fixture.calls().contains("--user stop droidloom.service"));
    assert!(!fixture.calls().contains("--user start droidloom.service"));
    assert_eq!(fs::read(fixture.record_path()).unwrap(), before);
}

#[test]
fn absent_wayland_and_status_do_not_mutate_a_session_record() {
    let fixture = SessionFixture::new();
    let output = fixture.command(&["start", "--no-wait"]).env_remove("WAYLAND_DISPLAY").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("WAYLAND_DISPLAY"));
    assert!(!fixture.record_path().exists());
    assert!(fixture.calls().is_empty());
    let directory = tempfile::tempdir().unwrap();
    for operation in ["status", "logs"] {
        let output = Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
            .args([operation, "--socket"]).arg(directory.path().join("missing.sock"))
            .env("XDG_RUNTIME_DIR", directory.path()).output().unwrap();
        assert!(!output.status.success());
        assert!(!directory.path().join("droidloom").exists());
    }
}

#[test]
fn service_failure_is_reported() {
    let fixture = SessionFixture::new();
    fs::write(fixture.directory.path().join("systemctl"), "#!/bin/sh\nexit 1\n").unwrap();
    let output = fixture.command(&["stop"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("stop failed"));
}

#[test]
fn internal_cell_operations_do_not_recurse_into_systemd() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
        .args(["stop", "--cell", "--socket"]).arg(directory.path().join("missing.sock"))
        .env("PATH", directory.path()).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("connect to droidloomd"));
}

#[test]
fn lifecycle_rejects_a_symlink_runtime_directory() {
    let fixture = SessionFixture::new();
    let target = tempfile::tempdir().unwrap();
    fs::set_permissions(target.path(), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(target.path(), fixture.directory.path().join("droidloom")).unwrap();
    let output = fixture.command(&["start"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("session-owned directory"));
    assert_eq!(fs::metadata(target.path()).unwrap().permissions().mode() & 0o777, 0o755);
    assert!(!target.path().join("session.env").exists());
    assert!(fixture.calls().is_empty());
}
