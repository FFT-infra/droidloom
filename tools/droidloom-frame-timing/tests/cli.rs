//! Exercise persisted reports and comparisons through the public CLI.
use std::{fs, process::Command};

#[test]
fn capture_round_trip_compare_and_missing_data() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("capture.log");
    let report = temp.path().join("report.json");
    fs::write(&log, "journal prefix Droidloom frame trace: pid=123 seq=1 stage=timing_presenter_commit object=2 frame=9 buffer=0 observed_ns=1020 start_ns=1000 wall_ns=20 cpu_ns=10 layers=3 outcome=1 dropped_total=0\n").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_droidloom-frame-timing"))
        .arg("summarize")
        .arg(&log)
        .args(["--label", "fixture", "--output"])
        .arg(&report)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    assert_eq!(json["timings"][0]["wall"]["p95_ns"], 20);
    assert_eq!(json["timings"][0]["cpu"]["p95_ns"], 10);
    let result = Command::new(env!("CARGO_BIN_EXE_droidloom-frame-timing"))
        .arg("compare")
        .arg(&report)
        .arg(&report)
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("1/1 | 0.0000→0.0000, +0.0000"));
    fs::write(&log, "unrelated log\n").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_droidloom-frame-timing"))
        .arg("summarize")
        .arg(&log)
        .args(["--label", "empty", "--output"])
        .arg(temp.path().join("empty.json"))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!temp.path().join("empty.json").exists());
}
