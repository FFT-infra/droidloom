//! APK CLI errors and output without installing applications on the host.
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;

#[test]
fn install_preserves_package_manager_results_in_text_and_json() {
    for (ok, json) in [(true, false), (true, true), (false, false), (false, true)] {
        let directory = tempfile::tempdir().unwrap();
        let apk = directory.path().join("test application.apk");
        std::fs::write(&apk, b"PK\x03\x04test payload").unwrap();
        let socket = directory.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Descriptor ownership/content are covered by the real receiver's
            // unit tests. read() discards the ancillary fd in this mock.
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).unwrap();
            let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                request,
                serde_json::json!({"command": "install", "user": 10})
            );
            stream.write_all(&serde_json::to_vec(&serde_json::json!({
                "ok": ok, "state": "running",
                "message": if ok { "APK installed successfully" } else { "Failure [INSTALL_FAILED_INVALID_APK]" }
            })).unwrap()).unwrap();
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_droidloomctl"));
        command
            .arg("--socket")
            .arg(socket)
            .arg("install")
            .arg(apk)
            .args(["--user", "10"]);
        if json {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        server.join().unwrap();
        assert_eq!(output.status.success(), ok, "{output:?}");
        if json {
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(response["ok"], ok);
        } else if ok {
            assert!(String::from_utf8_lossy(&output.stdout).contains("installed successfully"));
        } else {
            assert!(String::from_utf8_lossy(&output.stderr).contains("INSTALL_FAILED_INVALID_APK"));
        }
    }
}

#[test]
fn install_rejects_missing_and_invalid_apks_before_connecting() {
    let directory = tempfile::tempdir().unwrap();
    let apk = directory.path().join("bad.apk");
    for contents in [None, Some(b"not an apk".as_slice())] {
        if let Some(contents) = contents {
            std::fs::write(&apk, contents).unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
            .arg("--socket")
            .arg(directory.path().join("absent.sock"))
            .arg("install")
            .arg(&apk)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("APK"), "{error}");
        assert!(!error.contains("connect to droidloomd"));
    }
}
