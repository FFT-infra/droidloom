//! Exercises the pinned upstream libdrm against synthetic DRM kernel replies.
use std::{fs, path::Path, process::Command};

#[test]
#[ignore = "requires the pinned .work/arch/build/aosp-source libdrm checkout and host C compiler"]
fn drm_version_accepts_empty_kernel_strings() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = repo.join(".work/arch/build/aosp-source/external/libdrm");
    let temp = tempfile::tempdir().unwrap();
    let pristine = Command::new("git")
        .current_dir(&source)
        .args(["show", "HEAD:xf86drm.c"])
        .output()
        .unwrap();
    assert!(pristine.status.success());
    fs::write(temp.path().join("xf86drm.c"), pristine.stdout).unwrap();
    let apply = Command::new("git")
        .current_dir(temp.path())
        .args(["apply", "--include=xf86drm.c"])
        .arg(repo.join("android/aosp-patches/0015-libdrm-empty-version-strings.patch"))
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "{}",
        String::from_utf8_lossy(&apply.stderr)
    );
    let binary = temp.path().join("version-test");
    let compile = Command::new("cc")
        .args([
            "-std=gnu11",
            "-DMAJOR_IN_SYSMACROS",
            "-ffunction-sections",
            "-fdata-sections",
            "-Wl,--gc-sections",
            "-Wl,--wrap=ioctl",
            "-I",
        ])
        .arg(&source)
        .arg("-I")
        .arg(source.join("include/drm"))
        .arg(temp.path().join("xf86drm.c"))
        .arg(repo.join("android/mesa/tests/libdrm_version.c"))
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
