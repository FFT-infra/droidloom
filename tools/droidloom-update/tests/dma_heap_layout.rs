//! Requires the pinned minigbm checkout; runs allocator arithmetic without a device.
use std::{fs, path::Path, process::Command};

#[test]
#[ignore = "requires .work/aosp-m2-source and host libdrm development headers"]
fn dma_heap_video_planes_satisfy_gpu_and_android_layouts() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = repo.join(".work/aosp-m2-source/external/minigbm");
    let temp = tempfile::tempdir().unwrap();
    let pristine = Command::new("git")
        .current_dir(&source)
        .args(["show", "HEAD:dma_heap.c"])
        .output()
        .unwrap();
    assert!(pristine.status.success());
    fs::write(temp.path().join("dma_heap.c"), pristine.stdout).unwrap();
    let apply = Command::new("git")
        .current_dir(temp.path())
        .args(["apply", "--include=dma_heap.c"])
        .arg(repo.join("android/aosp-patches/0010-minigbm-dma-heap-images.patch"))
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "{}",
        String::from_utf8_lossy(&apply.stderr)
    );
    let binary = temp.path().join("layout-test");
    let compile = Command::new("cc")
        .args([
            "-std=gnu11",
            "-ffunction-sections",
            "-fdata-sections",
            "-Wl,--gc-sections",
            "-I/usr/include/libdrm",
            "-I",
        ])
        .arg(temp.path())
        .arg("-I")
        .arg(&source)
        .arg(repo.join("android/mesa/tests/dma_heap_layout.c"))
        .arg(source.join("drv_helpers.c"))
        .arg(source.join("drv.c"))
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
