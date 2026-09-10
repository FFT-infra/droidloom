//! Exercise the CLI/socket boundary without root or a running Android cell.
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;

#[test]
fn diagnostics_send_typed_requests_and_render_text_or_json() {
    for (args, crashes, json) in [
        (
            vec!["logs", "com.example.app", "-n", "42", "--user", "10"],
            false,
            false,
        ),
        (
            vec!["crashes", "com.example.app", "-n", "42", "--json"],
            true,
            true,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.sock");
        let listener = match UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("bind test socket: {error}"),
        };
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).unwrap();
            let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let request = &request["request"];
            assert_eq!(request["command"], "diagnostics");
            assert_eq!(request["package"], "com.example.app");
            assert_eq!(request["lines"], 42);
            assert_eq!(request["crashes"], crashes);
            assert_eq!(request["user"], if crashes { 0 } else { 10 });
            stream.write_all(br#"{"ok":true,"state":"running","message":"Android diagnostics","diagnostics":"FATAL EXCEPTION\nfull stack trace\n"}"#).unwrap();
        });
        let output = Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
            .args(["--socket"])
            .arg(socket)
            .args(args)
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(output.status.success(), "{output:?}");
        if json {
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                response["diagnostics"],
                "FATAL EXCEPTION\nfull stack trace\n"
            );
        } else {
            assert_eq!(output.stdout, b"FATAL EXCEPTION\nfull stack trace\n");
        }
    }
}

#[test]
fn invalid_limits_fail_before_connecting_to_the_daemon() {
    for lines in ["0", "2001"] {
        let output = Command::new(env!("CARGO_BIN_EXE_droidloomctl"))
            .args(["logs", "--lines", lines])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("connect to droidloomd"));
    }
}
