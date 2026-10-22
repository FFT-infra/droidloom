//! Android writable state lives directly on the host filesystem.
//!
//! Cells provisioned before that layout keep one detached raw ext4 image per
//! directory below the same parent. Both layouts stay readable so an existing
//! tablet cell keeps booting; only `droidloom-supervisor migrate-storage`
//! replaces images with directories, and it needs free space for a full copy.
//! See `docs/contracts/runtime-layout-v1.md`.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Names below `CellSpec::data_dir`, bind mounted at the matching Android paths.
pub const DIRECTORIES: [&str; 2] = ["data", "metadata"];

/// Names below `CellSpec::data_dir` in the superseded image layout, loop
/// mounted at the matching Android paths.
pub const LEGACY_IMAGES: [&str; 2] = ["data.img", "metadata.img"];

/// Writable storage layout found below one cell's data directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WritableStorage {
    /// Direct host directories; every new cell is provisioned this way.
    Directories,
    /// Detached raw ext4 images awaiting `migrate-storage`.
    LegacyImages,
}

fn is_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Detect one cell's layout without following links.
///
/// A parent carrying neither layout — a new cell, or an absent path during
/// planning — reports [`WritableStorage::Directories`] so provisioning and
/// planning stay on the current layout. A parent carrying both reports
/// directories as well; provisioning then rejects the ambiguity through
/// [`prepare`] instead of guessing which copy holds Android's state.
pub fn writable_storage(parent: &Path) -> WritableStorage {
    let directories = DIRECTORIES.iter().all(|name| is_directory(&parent.join(name)));
    let images = LEGACY_IMAGES.iter().all(|name| is_regular_file(&parent.join(name)));
    if directories || !images {
        WritableStorage::Directories
    } else {
        WritableStorage::LegacyImages
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub(crate) fn check_parent(parent: &Path) -> io::Result<()> {
    // Ancestors must prevent the desktop user from replacing any mount source.
    for path in parent.ancestors() {
        let m = fs::symlink_metadata(path)?;
        if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err(invalid(format!(
                "unsafe Android storage parent: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn reject_legacy(parent: &Path) -> io::Result<()> {
    for name in LEGACY_IMAGES {
        match fs::symlink_metadata(parent.join(name)) {
            Ok(_) => {
                return Err(invalid(format!(
                    "{} contains legacy Android images; stop Droidloom and use migrate-storage into a new directory before activation",
                    parent.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Validate the legacy image layout without following links.
pub fn validate_legacy(parent: &Path) -> io::Result<()> {
    check_parent(parent)?;
    for name in LEGACY_IMAGES {
        let path = parent.join(name);
        if !fs::symlink_metadata(&path)?.is_file() {
            return Err(invalid(format!(
                "{} is not a direct regular file",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Validate direct directories without following links or accepting old images.
/// Android owns the inner directory modes/IDs; the outer parent stays private.
pub fn validate(parent: &Path) -> io::Result<()> {
    check_parent(parent)?;
    reject_legacy(parent)?;
    for name in DIRECTORIES {
        let path = parent.join(name);
        if !fs::symlink_metadata(&path)?.is_dir() {
            return Err(invalid(format!(
                "{} is not a direct directory",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Provision a new directory layout; never replace images or reset existing data.
///
/// Each new directory starts like the root of a freshly formatted image
/// (0755, root): Android's system services must traverse `/metadata` before
/// init adjusts anything below it, or aconfigd cannot create the flag storage
/// and system_server aborts. The private parent keeps host users out.
pub fn prepare(parent: &Path) -> io::Result<()> {
    check_parent(parent)?;
    if writable_storage(parent) == WritableStorage::LegacyImages {
        // The images own this cell's writable state. Creating directories here
        // would split it, so leave the parent untouched until the operator
        // migrates it with enough free space.
        return Ok(());
    }
    reject_legacy(parent)?;
    for name in DIRECTORIES {
        let path = parent.join(name);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => (),
            Ok(_) => {
                return Err(invalid(format!(
                    "unsafe Android storage: {}",
                    path.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&path)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
            }
            Err(e) => return Err(e),
        }
    }
    fs::File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_storage_is_never_silently_reset() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("data.img"), b"existing user data").unwrap();
        assert!(reject_legacy(root.path()).is_err());
        assert!(!root.path().join("data").exists());
        assert_eq!(
            fs::read(root.path().join("data.img")).unwrap(),
            b"existing user data"
        );
    }

    #[test]
    fn layout_detection_defaults_to_directories_and_accepts_complete_images() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(writable_storage(root.path()), WritableStorage::Directories);
        fs::write(root.path().join("data.img"), b"image").unwrap();
        assert_eq!(writable_storage(root.path()), WritableStorage::Directories);
        fs::write(root.path().join("metadata.img"), b"image").unwrap();
        assert_eq!(writable_storage(root.path()), WritableStorage::LegacyImages);
        for name in DIRECTORIES {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        assert_eq!(writable_storage(root.path()), WritableStorage::Directories);
    }

    #[test]
    fn provisioning_leaves_legacy_images_and_a_fresh_parent_on_their_layout() {
        let root = tempfile::tempdir().unwrap();
        if check_parent(root.path()).is_err() {
            // `prepare` refuses any parent the desktop user could replace, so
            // the assertions need a privileged runner with a private TMPDIR.
            eprintln!("TMPDIR is not root-owned and private; skipping provisioning assertions");
            return;
        }
        fs::write(root.path().join("data.img"), b"existing user data").unwrap();
        fs::write(root.path().join("metadata.img"), b"existing metadata").unwrap();
        prepare(root.path()).unwrap();
        assert!(!root.path().join("data").exists());
        assert_eq!(
            fs::read(root.path().join("data.img")).unwrap(),
            b"existing user data"
        );
        validate_legacy(root.path()).unwrap();

        let fresh = tempfile::tempdir().unwrap();
        prepare(fresh.path()).unwrap();
        for name in DIRECTORIES {
            assert!(fresh.path().join(name).is_dir());
        }
    }
}
