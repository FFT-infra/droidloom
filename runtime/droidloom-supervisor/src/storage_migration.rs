//! Offline, non-destructive conversion of old writable images to directories.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn run(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{command:?} failed: {status}").into());
    }
    Ok(())
}

fn check_source(source: &Path) -> Result<()> {
    for name in ["data.img", "metadata.img"] {
        let image = source.join(name);
        let m = fs::symlink_metadata(&image)?;
        if !m.is_file() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err(format!("unsafe legacy image {}", image.display()).into());
        }
        let loops = Command::new("losetup")
            .args(["--noheadings", "--output", "NAME", "--associated"])
            .arg(&image)
            .output()?;
        if !loops.status.success() || !loops.stdout.is_empty() {
            return Err(format!(
                "{} is attached or its status cannot be verified; stop Droidloom first",
                image.display()
            )
            .into());
        }
        // Read-only check: never repair or mutate the user's original image.
        run(Command::new("e2fsck").arg("-fn").arg(&image))?;
    }
    Ok(())
}

/// Copy an inactive legacy layout, preserving contents, IDs, xattrs and links.
/// The originals and cell configuration are never modified. Runs in a private
/// mount namespace and publishes the destination only after checksum validation.
pub fn migrate(source: &Path, destination: &Path, worker: bool) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("storage migration requires root".into());
    }
    if !source.is_absolute() || !destination.is_absolute() || destination.exists() {
        return Err("use an absolute source and a new absolute destination".into());
    }
    crate::host_storage::check_parent(source)?;
    let parent = destination.parent().ok_or("destination lacks parent")?;
    crate::host_storage::check_parent(parent)?;
    if !worker {
        return run(Command::new("unshare")
            .args(["--mount", "--propagation", "private"])
            .arg(std::env::current_exe()?)
            .arg("migrate-storage")
            .arg("--source")
            .arg(source)
            .arg("--destination")
            .arg(destination)
            .arg("--private-worker"));
    }
    if fs::read_link("/proc/self/ns/mnt")? == fs::read_link("/proc/1/ns/mnt")? {
        return Err("migration worker requires its private mount namespace".into());
    }
    check_source(source)?;
    let staging = tempfile::Builder::new()
        .prefix(".storage-migration-")
        .tempdir_in(parent)?;
    fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700))?;
    let output = staging.path().join("storage");
    fs::create_dir(&output)?;
    fs::set_permissions(&output, fs::Permissions::from_mode(0o700))?;
    for name in crate::host_storage::DIRECTORIES {
        let image = source.join(format!("{name}.img"));
        let mount = staging.path().join(format!("source-{name}"));
        let target = output.join(name);
        fs::create_dir(&mount)?;
        fs::create_dir(&target)?;
        run(Command::new("mount")
            .args(["-t", "ext4", "-o", "loop,ro,noload,nodev,nosuid"])
            .arg(&image)
            .arg(&mount))?;
        let copy = (|| -> Result<()> {
            let from = format!("{}/", mount.display());
            let to = format!("{}/", target.display());
            run(Command::new("rsync").args([
                "-aHAX",
                "--numeric-ids",
                "--one-file-system",
                "--",
                &from,
                &to,
            ]))?;
            let verify = Command::new("rsync")
                .args([
                    "-aHAXcni",
                    "--delete",
                    "--numeric-ids",
                    "--one-file-system",
                    "--",
                    &from,
                    &to,
                ])
                .output()?;
            if !verify.status.success() || !verify.stdout.is_empty() {
                return Err(format!(
                    "copy verification failed: {} {}",
                    String::from_utf8_lossy(&verify.stdout),
                    String::from_utf8_lossy(&verify.stderr)
                )
                .into());
            }
            run(Command::new("sync").arg("-f").arg(&target))?;
            Ok(())
        })();
        // Unmount even on a copy error; the namespace also dies on process exit.
        let unmount = run(Command::new("umount").arg(&mount));
        if unmount.is_err() {
            // Never recursively remove a directory with a live source mount.
            let _ = staging.keep();
            return Err("migration mount could not be detached; staging retained".into());
        }
        copy?;
        eprintln!("{name}: copied and checksum/metadata verified");
    }
    crate::host_storage::validate(&output)?;
    // RENAME_NOREPLACE ensures an existing destination can never be overwritten.
    let old = std::ffi::CString::new(output.as_os_str().as_encoded_bytes())?;
    let new = std::ffi::CString::new(destination.as_os_str().as_encoded_bytes())?;
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    fs::File::open(parent)?.sync_all()?;
    println!(
        "Verified host storage: {}. Original images and configuration preserved.",
        destination.display()
    );
    Ok(())
}
