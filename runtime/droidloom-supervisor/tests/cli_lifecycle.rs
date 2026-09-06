//! Verify complete service orchestration and the internal recovery boundary.
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn lifecycle_controls_the_whole_user_service() {
    for operation in ["start", "stop", "restart"] {
        let directory = tempfile::tempdir().unwrap();
        let systemctl = directory.path().join("systemctl");
        fs::write(
            &systemctl,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CALL_LOG\"\n",
        )
        .unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
        let log = directory.path().join("calls");
        let output = Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
            .args([operation, "--json"])
            .env("PATH", directory.path())
            .env("CALL_LOG", &log)
            .env("XDG_RUNTIME_DIR", directory.path())
            .env("WAYLAND_DISPLAY", "wayland-test")
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        if operation != "stop" {
            assert_eq!(fs::metadata(directory.path().join("droidloom")).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains(&format!("--user {operation} droidloom.service")));
        assert_eq!(calls.contains("import-environment"), operation != "stop");
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            response["state"],
            if operation == "stop" {
                "stopped"
            } else {
                "running"
            }
        );
    }
}

#[test]
fn service_failure_is_reported() {
    let directory = tempfile::tempdir().unwrap();
    let systemctl = directory.path().join("systemctl");
    fs::write(&systemctl, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
        .arg("stop")
        .env("PATH", directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("stop failed"));
}

#[test]
fn internal_cell_operations_do_not_recurse_into_systemd() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
        .args(["stop", "--cell", "--socket"])
        .arg(directory.path().join("missing.sock"))
        .env("PATH", directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("connect to droidloomd"));
}

#[test]
fn explicit_mode_reaches_service_and_restart_preserves_it() {
    let directory = tempfile::tempdir().unwrap();
    let systemctl = directory.path().join("systemctl");
    fs::write(&systemctl, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CALL_LOG\"\n").unwrap();
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
    let log = directory.path().join("calls");
    let run = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
        .args(args).env("PATH", directory.path()).env("CALL_LOG", &log)
        .env("XDG_RUNTIME_DIR", directory.path()).output().unwrap();
    assert!(run(&["start", "--mode", "mobile"]).status.success());
    let mode_file = directory.path().join("droidloom/session.env");
    assert_eq!(fs::read_to_string(&mode_file).unwrap(), "DROIDLOOM_MODE=mobile\n");
    assert!(fs::read_to_string(&log).unwrap().contains("--user restart droidloom.service"));
    fs::set_permissions(mode_file.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(run(&["restart"]).status.success());
    assert_eq!(fs::metadata(mode_file.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::read_to_string(&mode_file).unwrap(), "DROIDLOOM_MODE=mobile\n");
    assert!(run(&["start"]).status.success());
    assert_eq!(fs::read_to_string(&mode_file).unwrap(), "DROIDLOOM_MODE=desktop\n");
    assert!(!run(&["start", "--mode", "tablet"]).status.success());
}

#[test]
fn lifecycle_rejects_a_symlink_runtime_directory() {
    let runtime = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    fs::set_permissions(target.path(), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(target.path(), runtime.path().join("droidloom")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
        .arg("start")
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("PATH", runtime.path())
        .output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("session-owned directory"));
    assert_eq!(fs::metadata(target.path()).unwrap().permissions().mode() & 0o777, 0o755);
    assert!(!target.path().join("session.env").exists());
}
