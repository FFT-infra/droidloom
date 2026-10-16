//! Tests the prepared pinned Mesa model lookup against real Mali GPU IDs.
use std::{path::Path, process::Command};

#[test]
#[ignore = "requires prepared .work/arch/build/mesa-source and a host C compiler"]
fn panfrost_recognizes_the_tablet_gpu_without_changing_other_models() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = repo.join(".work/arch/build/mesa-source");
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("model-test");
    let compile = Command::new("cc")
        .args(["-std=gnu11", "-I"])
        .arg(source.join("src"))
        .arg(source.join("src/panfrost/model/pan_model.c"))
        .arg(repo.join("android/mesa/tests/panfrost_model.c"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    assert!(Command::new(binary).status().unwrap().success());
}
