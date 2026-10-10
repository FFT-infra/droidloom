//! Compile the actual new SurfaceFlinger methods with isolated framework fakes.
use std::{fs, path::Path, process::Command};

fn block(source: &str, start: &str) -> String {
    let source = &source[source.find(start).expect("production block is in patch")..];
    let opening = source.find('{').unwrap();
    let mut depth = 0;
    for (index, byte) in source.bytes().enumerate().skip(opening) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return source[..=index].to_owned();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated production block");
}

#[test]
fn headless_present_fence_retains_ownership_and_completion() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("headless-fence-test");
    let result = Command::new("rustc")
        .args(["--edition=2021", "--test"])
        .arg(repo.join("graphics/droidloom-composer-aidl/src/headless_fence.rs"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        Command::new("timeout")
            .arg("10s")
            .arg(binary)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn bufferless_completion_preserves_task_fences_and_pending_buffer_fallback() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let patch = fs::read_to_string(
        repo.join("android/surfaceflinger/0017-droidloom-bufferless-bootstrap.patch"),
    )
    .unwrap();
    let added = patch
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .map(|line| format!("{}\n", &line[1..]))
        .collect::<String>();
    let methods = format!(
        "{}\n{}\n{}\n{}\nvoid Output::finishFrame(GpuCompositionResult&& result) {{\n\
         const auto& outputState = getState();\n\
         if (!outputState.isEnabled) return;\n{}\n++fallbackFrames;\n}}\n",
        block(&added, "bool Output::finishDroidloomHeadlessFrame()"),
        block(&added, "void Output::prepareDroidloomHeadlessFrame()"),
        block(&added, "bool RenderSurface::prepareDroidloomHeadlessFrame("),
        block(&added, "bool RenderSurface::queueDroidloomHeadlessFrame("),
        block(
            &added,
            "if (outputState.strategyPrediction == CompositionStrategyPredictionState::DISABLED",
        ),
    );
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("DroidloomHeadlessMethods.inc"), methods).unwrap();
    let binary = temp.path().join("bufferless-bootstrap-test");
    let result = Command::new("c++")
        .args(["-std=c++20", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(temp.path())
        .arg(repo.join("android/surfaceflinger/tests/bufferless_bootstrap.cpp"))
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("C++ compiler required for SurfaceFlinger regression test");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let status = Command::new("timeout")
        .arg("10s")
        .arg(binary)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "headless completion failed or waited: {status}"
    );
}
