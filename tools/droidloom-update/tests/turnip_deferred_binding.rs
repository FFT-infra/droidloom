//! Compile the actual Android bind branch from pinned Mesa, after applying our
//! patch. This catches missing imports, reversed layout/bind order and errors
//! swallowed before the live Android swapchain test.
use std::{fs, path::Path, process::Command};

#[test]
#[ignore = "requires a prepared Mesa source and a host C++ compiler"]
fn android_deferred_swapchain_imports_layout_before_binding_memory() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = std::env::var_os("DROIDLOOM_MESA_SOURCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| repo.join(".work/update/mesa-source"));
    let temporary = tempfile::tempdir().unwrap();
    let image = temporary.path().join("src/freedreno/vulkan/tu_image.cc");
    fs::create_dir_all(image.parent().unwrap()).unwrap();
    fs::copy(source.join("src/freedreno/vulkan/tu_image.cc"), &image).unwrap();
    if std::env::var_os("DROIDLOOM_TEST_UNPATCHED").is_none() {
        let output = Command::new("patch")
            .current_dir(temporary.path())
            .args(["--batch", "-p1", "-i"])
            .arg(repo.join("android/aosp-patches/0034-turnip-android-deferred-binding.patch"))
            .output()
            .unwrap();
        assert!(output.status.success(), "patch failed: {output:?}");
    }
    let text = fs::read_to_string(&image).unwrap();
    let function = text.split_once("\ntu_image_bind(").unwrap().1;
    let branch = function
        .split_once("#if DETECT_OS_ANDROID\n")
        .unwrap()
        .1
        .split_once("#else")
        .unwrap()
        .0;
    assert!(!branch.trim().is_empty());
    let harness = fs::read_to_string(repo.join("android/mesa/tests/turnip_deferred_binding.cpp"))
        .unwrap()
        .replace("/* ACTUAL_ANDROID_BIND_BRANCH */", branch);
    let translation_unit = temporary.path().join("binding.cpp");
    fs::write(&translation_unit, harness).unwrap();
    let executable = temporary.path().join("binding-test");
    let output = Command::new("c++")
        .args(["-std=c++20", "-Wall", "-Wextra"])
        .arg(&translation_unit)
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(output.status.success(), "compile failed: {output:?}");
    let output = Command::new(executable).output().unwrap();
    assert!(output.status.success(), "bind regression: {output:?}");
}
