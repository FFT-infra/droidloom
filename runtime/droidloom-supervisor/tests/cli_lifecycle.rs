//! Verify complete service orchestration and the internal recovery boundary.
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

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
                        (
                            false,
                            "Android looks stuck. Check the logs with: journalctl -b -u droidloomd.service",
                        )
                    }
                };
                let mut response = serde_json::to_vec(
                    &serde_json::json!({"ok": ok, "state": "running", "message": message}),
                )
                .unwrap();
                response.push(b'\n');
                stream.write_all(&response).unwrap();
            }
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_droidloomctl"));
        command.args(["--socket"]).arg(socket).arg("start");
        if json {
            command.arg("--json");
        }
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
            assert!(
                stdout.is_empty(),
                "failed boot must not print a running/ready success"
            );
            assert!(stderr.contains("Android looks stuck"));
            assert!(stderr.contains("journalctl -b -u droidloomd.service"));
        }
    }
}

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
        let mut command = Command::new(env!("CARGO_BIN_EXE_droidloomctl"));
        command.args([operation, "--json"]);
        if operation != "stop" {
            command.arg("--no-wait");
        }
        let output = command
            .env("PATH", directory.path())
            .env("CALL_LOG", &log)
            .env("XDG_RUNTIME_DIR", directory.path())
            .env("WAYLAND_DISPLAY", "wayland-test")
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        if operation != "stop" {
            assert_eq!(
                fs::metadata(directory.path().join("droidloom"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
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
    fs::write(
        &systemctl,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CALL_LOG\"\n",
    )
    .unwrap();
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
    let log = directory.path().join("calls");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
            .args(args)
            .arg("--no-wait")
            .env("PATH", directory.path())
            .env("CALL_LOG", &log)
            .env("XDG_RUNTIME_DIR", directory.path())
            .output()
            .unwrap()
    };
    assert!(run(&["start", "--mode", "mobile"]).status.success());
    let mode_file = directory.path().join("droidloom/session.env");
    assert_eq!(
        fs::read_to_string(&mode_file).unwrap(),
        "DROIDLOOM_MODE=mobile\n"
    );
    assert!(
        fs::read_to_string(&log)
            .unwrap()
            .contains("--user restart droidloom.service")
    );
    fs::set_permissions(
        mode_file.parent().unwrap(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(run(&["restart"]).status.success());
    assert_eq!(
        fs::metadata(mode_file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::read_to_string(&mode_file).unwrap(),
        "DROIDLOOM_MODE=mobile\n"
    );
    assert!(run(&["start"]).status.success());
    assert_eq!(
        fs::read_to_string(&mode_file).unwrap(),
        "DROIDLOOM_MODE=desktop\n"
    );
    assert!(!run(&["start", "--mode", "tablet"]).status.success());
}

#[test]
fn lifecycle_rejects_a_symlink_runtime_directory() {
    let runtime = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    // Package-installed hosts reload the user manager before validating this
    // path. Keep that preparatory command isolated, as in the lifecycle tests.
    let systemctl = runtime.path().join("systemctl");
    fs::write(&systemctl, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(target.path(), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(target.path(), runtime.path().join("droidloom")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
        .arg("start")
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("PATH", runtime.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("session-owned directory"));
    assert_eq!(
        fs::metadata(target.path()).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!target.path().join("session.env").exists());
}
