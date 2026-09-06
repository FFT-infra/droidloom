//! SystemUI's APK is overlaid inside an immutable directory. Android's parser
//! cache checks that directory's mtime, so it cannot notice the replaced APK.
//! Invalidate only this package on every boot, including rollback to stock.

use std::{fs, io, path::Path};

pub(crate) fn invalidate_systemui_cache(root: &Path) -> io::Result<usize> {
    fn visit(path: &Path) -> io::Result<usize> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        if !metadata.is_dir() {
            return Ok(0);
        }
        let mut removed = 0;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                removed += visit(&entry.path())?;
            } else if kind.is_file() && entry.file_name().to_str().is_some_and(|name| {
                name.starts_with("SystemUI-") || name.starts_with("SystemUI.apk-")
            }) {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }
    visit(&root.join("data/system/package_cache"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn drops_stale_systemui_manifests_without_touching_other_apps_or_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("data/system/package_cache/37");
        fs::create_dir_all(&cache).unwrap();
        for name in ["SystemUI-16-123", "SystemUI.apk-16-456", "Settings-16-789"] {
            fs::write(cache.join(name), "old parsed manifest").unwrap();
        }
        let external = tempfile::tempdir().unwrap();
        fs::write(external.path().join("SystemUI-16-123"), "outside cache").unwrap();
        symlink(external.path(), cache.join("linked-directory")).unwrap();
        symlink(external.path().join("SystemUI-16-123"), cache.join("SystemUI-linked")).unwrap();
        assert_eq!(invalidate_systemui_cache(root.path()).unwrap(), 2);
        assert!(cache.join("Settings-16-789").exists());
        assert!(cache.join("SystemUI-linked").exists());
        assert!(external.path().join("SystemUI-16-123").exists());
        assert_eq!(invalidate_systemui_cache(root.path()).unwrap(), 0);
    }

    #[test]
    fn clean_data_image_needs_no_cache_directory() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(invalidate_systemui_cache(root.path()).unwrap(), 0);
        assert!(!root.path().join("data").exists());
    }
}
