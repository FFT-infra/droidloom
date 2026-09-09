//! Exercise the actual C++ release scheduler embedded in the SurfaceFlinger patch.
use std::{fs, path::Path, process::Command};

#[test]
fn layer_pipeline_uses_one_commit_reply_and_preserves_legacy_staging() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let temp = tempfile::tempdir().unwrap();
    // Materialize the actual final transport after both patches, checking that
    // the modified new-file patch still composes with the release-fence patch.
    for patch in [
        "0007-droidloom-wayland-layers.patch",
        "0008-droidloom-release-fences.patch",
    ] {
        let output = Command::new("git")
            .current_dir(temp.path())
            .args([
                "apply",
                "--include=services/surfaceflinger/DroidloomLayers.cpp",
                "--include=services/surfaceflinger/DroidloomLayers.h",
            ])
            .arg(repo.join("android/surfaceflinger").join(patch))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let cpp = fs::read_to_string(
        temp.path()
            .join("services/surfaceflinger/DroidloomLayers.cpp"),
    )
    .unwrap();
    let start = cpp.find("uint32_t u32(").unwrap();
    let end = cpp.find("// Exact ABI of the pinned minigbm").unwrap();
    fs::write(
        temp.path().join("DroidloomLayerTransport.inc"),
        &cpp[start..end],
    )
    .unwrap();
    let binary = temp.path().join("layer-staging-test");
    let output = Command::new("c++")
        .args([
            "-std=c++20",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-pthread",
            "-I",
        ])
        .arg(temp.path())
        .arg(repo.join("android/surfaceflinger/tests/layer_staging.cpp"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = Command::new("timeout")
        .arg("15s")
        .arg(binary)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "layer pipeline failed or deadlocked: {status}"
    );
}

#[test]
fn host_fences_do_not_block_other_apps_or_commit_callbacks() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let patch =
        fs::read_to_string(repo.join("android/surfaceflinger/0008-droidloom-release-fences.patch"))
            .unwrap();
    let section = patch
        .split("diff --git ")
        .find(|section| section.starts_with("a/services/surfaceflinger/DroidloomReleaseQueue.h "))
        .expect("release scheduler is part of the Android patch");
    let header: String = section
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .map(|line| format!("{}\n", &line[1..]))
        .collect();
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("DroidloomReleaseQueue.h"), header).unwrap();
    let binary = temp.path().join("release-queue-test");
    let output = Command::new("c++")
        .args([
            "-std=c++20",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-pthread",
            "-I",
        ])
        .arg(temp.path())
        .arg(repo.join("android/surfaceflinger/tests/release_queue.cpp"))
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("C++ compiler required for SurfaceFlinger regression test");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = Command::new("timeout")
        .arg("15s")
        .arg(binary)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "release queue regression failed or deadlocked: {status}"
    );
}
