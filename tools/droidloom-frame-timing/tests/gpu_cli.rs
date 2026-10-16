use std::{
    fs,
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[test]
fn manual_capture_stops_without_signals_and_missing_counters_are_not_zero_usage() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("capture");
    let marker = temp.path().join("stop");
    let mut child = Command::new(env!("CARGO_BIN_EXE_droidloom-gpu-usage"))
        .args([
            "capture",
            "--process",
            &format!("test={}", std::process::id()),
            "--seconds",
            "60",
            "--interval-ms",
            "100",
            "--label",
            "no GPU device needed",
            "--output",
        ])
        .arg(&output)
        .arg("--stop-file")
        .arg(&marker)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !output.join("samples.jsonl").exists() && start.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(5));
    }
    fs::write(&marker, "stop requested by test\n").unwrap();
    while child.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(5));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        panic!("capture did not honor stop marker");
    }
    let result = child.wait_with_output().unwrap();
    // This test process has no GPU handles. The raw/report files survive, but
    // the command must say unavailable instead of declaring zero GPU usage.
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("no readable DRM clients"));
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("report.json")).unwrap()).unwrap();
    assert!(report["engines"].as_array().unwrap().is_empty());
    assert!(!report["samples"].as_array().unwrap().is_empty());
    let before = fs::read(output.join("samples.jsonl")).unwrap();
    fs::remove_file(&marker).unwrap();
    let second = Command::new(env!("CARGO_BIN_EXE_droidloom-gpu-usage"))
        .args([
            "capture",
            "--process",
            &format!("test={}", std::process::id()),
            "--seconds",
            "1",
            "--label",
            "must retain capture",
            "--output",
        ])
        .arg(&output)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert_eq!(fs::read(output.join("samples.jsonl")).unwrap(), before);
}
