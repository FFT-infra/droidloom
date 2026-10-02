//! Host prerequisites are installed through the host package manager, without shell scripts.
use crate::util::*;
use std::{collections::BTreeSet, fs, path::PathBuf, process::Command};
fn available(name: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|p| p.join(name).is_file())
}
pub fn ensure() -> Result<()> {
    let arch = PathBuf::from("/etc/arch-release").exists();
    let mut packages = BTreeSet::new();
    for (command, package) in [
        ("git", "git"),
        ("rsync", "rsync"),
        ("curl", "curl"),
        ("unzip", "unzip"),
        ("zip", "zip"),
        ("meson", "meson"),
        ("ninja", "ninja"),
        ("pkg-config", "pkgconf"),
        ("gst-inspect-1.0", "gstreamer"),
        ("cc", "base-devel"),
        ("llvm-config", "llvm"),
        ("bison", "bison"),
        ("flex", "flex"),
        ("simg2img", "android-tools"),
        ("lpunpack", "android-tools"),
        ("fsck.erofs", "erofs-utils"),
        ("mkfs.erofs", "erofs-utils"),
        ("fakeroot", "fakeroot"),
        ("getfattr", "attr"),
        ("dump.erofs", "erofs-utils"),
        ("mkfs.ext4", "e2fsprogs"),
        ("pkexec", "polkit"),
        ("ip", "iproute2"),
        ("nft", "nftables"),
        ("nsenter", "util-linux"),
        ("patch", "patch"),
        ("tar", "tar"),
    ] {
        if !available(command) {
            if !arch {
                return fail(format!(
                    "missing host prerequisite {command}; automatic package installation currently supports Arch-family build hosts"
                ));
            }
            packages.insert(package);
        }
    }
    for (library, package) in [
        ("gbm", "mesa"),
        ("libdrm", "libdrm"),
        ("wayland-client", "wayland"),
        ("gstreamer-app-1.0", "gst-plugins-base-libs"),
        ("gstreamer-video-1.0", "gst-plugins-base-libs"),
        ("LLVMSPIRVLib", "spirv-llvm-translator"),
    ] {
        if !available("pkg-config")
            || !Command::new("pkg-config")
                .args(["--exists", library])
                .status()?
                .success()
        {
            if !arch {
                return fail(format!(
                    "missing host development library {library}; automatic package installation currently supports Arch-family build hosts"
                ));
            }
            packages.insert(package);
        }
    }
    for (plugin, package) in [("pipewire", "gst-plugin-pipewire"), ("videorate", "gst-plugins-base")] {
        if !available("gst-inspect-1.0")
            || !Command::new("gst-inspect-1.0").arg(plugin).stdout(std::process::Stdio::null()).status()?.success()
        {
            if !arch {
                return fail(format!("missing GStreamer plugin {plugin}; install the host PipeWire and GStreamer base plugins"));
            }
            packages.insert(package);
        }
    }
    if arch && !Command::new("pacman").args(["-Qq", "pipewire-libcamera"])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()?.success()
    {
        packages.insert("pipewire-libcamera");
    }
    if !packages.is_empty() {
        eprintln!(
            "Installing required build/runtime packages: {}",
            packages.iter().copied().collect::<Vec<_>>().join(", ")
        );
        let authentication = if available("pkexec") {
            "pkexec"
        } else {
            "sudo"
        };
        run(Command::new(authentication)
            .arg("/usr/bin/pacman")
            .args(["-S", "--needed", "--noconfirm"])
            .args(packages))?;
    }
    if !fs::metadata("/run/systemd/system").is_ok_and(|m| m.is_dir()) {
        return fail("Droidloom service installation requires a running systemd host");
    }
    Ok(())
}
