//! Exercise the production Vulkan probe without loading a real graphics driver.
use std::{fs, path::Path, process::Command};

fn checked(command: &mut Command) {
    let result = command.output().expect("test build tool is installed");
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
#[ignore = "requires pinned AOSP Vulkan headers and a host C++ compiler"]
fn hardware_vulkan_capabilities_follow_the_active_android_loader() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let policy = repo.join("android/framework/droidloom-window-policy");
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("vulkan-capabilities-test");
    checked(
        Command::new("c++")
            .args(["-std=c++20", "-Wall", "-Wextra", "-Werror", "-I"])
            .arg(repo.join(".work/arch/build/aosp-source/external/vulkan-headers/include"))
            .arg("-I")
            .arg(policy.join("jni"))
            .arg(policy.join("jni/DroidloomVulkan.cpp"))
            .arg(policy.join("tests/vulkan_capabilities.cpp"))
            .arg("-o")
            .arg(&binary),
    );
    checked(Command::new("timeout").arg("10s").arg(binary));
}

#[test]
#[ignore = "requires a host JDK"]
fn java_vulkan_probe_boundary_fails_without_publishing_features() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let policy = repo.join("android/framework/droidloom-window-policy");
    let temp = tempfile::tempdir().unwrap();
    let stub = temp.path().join("android/util/Slog.java");
    fs::create_dir_all(stub.parent().unwrap()).unwrap();
    fs::write(
        &stub,
        "package android.util; public final class Slog {\n\
         public static int i(String tag, String message) { return 0; }\n\
         public static int w(String tag, String message, Throwable failure) { return 0; }\n}\n",
    )
    .unwrap();
    checked(
        Command::new("javac")
            .arg("-d")
            .arg(temp.path())
            .arg(&stub)
            .arg(policy.join("java/com/android/server/DroidloomGraphicsCapabilities.java"))
            .arg(policy.join("tests/com/android/server/GraphicsCapabilitiesTest.java")),
    );
    checked(
        Command::new("timeout")
            .args(["10s", "java", "-cp"])
            .arg(temp.path())
            .arg("com.android.server.GraphicsCapabilitiesTest"),
    );
}
