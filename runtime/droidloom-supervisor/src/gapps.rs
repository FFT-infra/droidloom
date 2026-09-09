//! Explicit, pre-boot selection of optional, base-bound Google-app images.
use crate::CellSpec;
use droidloom_contracts::{
    TARGET_ARCHITECTURE,
    gapps::{ADDON_ROLES, CORE_APPS, FileDigest, Manifest, SYNC_APPS},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const STATE: &str = ".droidloom-gapps.json";

/// Choose only product/`system_ext` from the add-on; system always uses the base.
pub(crate) fn partition_image(spec: &CellSpec, role: &str) -> PathBuf {
    let directory = if ADDON_ROLES.contains(&role) {
        spec.gapps_dir.as_ref().unwrap_or(&spec.image_dir)
    } else {
        &spec.image_dir
    };
    directory.join("images").join(format!("{role}.img"))
}

fn trusted_file(path: &Path) -> Result<()> {
    // Every ancestor must be root-owned and not writable by another identity.
    // Reject symlinks in the add-on tree so validation and mount select the
    // same package-owned names throughout the pre-boot transaction.
    for entry in path.ancestors() {
        let meta = fs::symlink_metadata(entry)?;
        if meta.file_type().is_symlink() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err(format!(
                "GApps path must be root-owned, without writable ancestors or symlinks: {}",
                entry.display()
            )
            .into());
        }
    }
    if !fs::metadata(path)?.is_file() {
        return Err("GApps input must be a regular file".into());
    }
    Ok(())
}

/// Check immutable add-on ownership, architecture and exact base/output hashes.
///
/// # Errors
/// Rejects missing, mutable or incompatible add-on inputs before Android starts.
pub fn validate(spec: &CellSpec) -> Result<()> {
    let Some(directory) = &spec.gapps_dir else {
        return Ok(());
    };
    trusted_file(&directory.join("manifest.json"))?;
    for role in ADDON_ROLES {
        trusted_file(&partition_image(spec, role))?;
    }
    let manifest = Manifest::load(&directory.join("manifest.json"))?;
    manifest.verify_images(&spec.image_dir, directory, TARGET_ARCHITECTURE)
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DataState {
    schema_version: u32,
    manifest: FileDigest,
    signers: BTreeMap<String, Vec<String>>,
}

/// Guard data transitions and refresh only Google apps' parser caches.
/// Called after mounting cell-owned /data and before Android init executes.
pub(crate) fn prepare_data(spec: &CellSpec, root: &Path) -> Result<()> {
    let current = spec
        .gapps_dir
        .as_ref()
        .map(|directory| -> Result<DataState> {
            let path = directory.join("manifest.json");
            let manifest = Manifest::load(&path)?;
            Ok(DataState {
                schema_version: 1,
                manifest: FileDigest::read(&path)?,
                signers: manifest
                    .apks
                    .into_iter()
                    .map(|apk| (apk.package, apk.signer_sha256))
                    .collect(),
            })
        })
        .transpose()?;
    reconcile_data(&root.join("data"), current)
}

fn reconcile_data(data: &Path, current: Option<DataState>) -> Result<()> {
    let state = data.join(STATE);
    let previous: Option<DataState> = match fs::symlink_metadata(&state) {
        Ok(meta) => {
            if !meta.is_file() || meta.len() > 65_536 {
                return Err("invalid GApps data state".into());
            }
            let mut bytes = Vec::new();
            fs::File::open(&state)?
                .take(65_537)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 65_536 {
                return Err("GApps data state exceeds limit".into());
            }
            Some(serde_json::from_slice(&bytes)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if previous.as_ref().is_some_and(|s| s.schema_version != 1) {
        return Err("unsupported GApps data state".into());
    }
    if previous.is_none() && current.is_none() {
        return Ok(());
    }
    // Intermediate symlinks must not redirect cache work outside the mounted
    // data filesystem before chroot. Android is stopped throughout this step.
    match fs::symlink_metadata(data.join("system")) {
        Ok(meta) if !meta.is_dir() => {
            return Err("Android /data/system must be a real directory".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let initialized = match fs::symlink_metadata(data.join("system/packages.xml")) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    match (&previous, &current) {
        (None, None) => return Ok(()),
        (Some(_), None) => return Err("this Android data was initialized with GApps; restore a pre-GApps backup or select fresh data before disabling it. Removing the image package does not remove Google APK updates or accounts from /data".into()),
        (None, Some(_)) if initialized => {
            return Err("first GApps activation requires fresh Android data; existing accounts and applications were preserved".into());
        }
        (Some(previous), Some(current)) if previous.signers != current.signers => {
            return Err("GApps application selection or signing certificates changed; use fresh data or a separately validated migration".into());
        }
        _ => {}
    }
    let current = current.ok_or("missing current GApps state")?;
    if previous.as_ref() == Some(&current) {
        return Ok(());
    }
    invalidate_cache(&data.join("system/package_cache"))?;
    let temporary = data.join(format!(".droidloom-gapps-{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(&serde_json::to_vec(&current)?)?;
        file.sync_all()?;
        fs::rename(&temporary, &state)?;
        fs::File::open(data)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn invalidate_cache(directory: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(directory) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !meta.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            invalidate_cache(&entry.path())?;
        } else if kind.is_file()
            && entry.file_name().to_str().is_some_and(|name| {
                CORE_APPS.into_iter().chain(SYNC_APPS).any(|(_, path)| {
                    let apk = Path::new(path).file_name().unwrap().to_str().unwrap();
                    let stem = apk.strip_suffix(".apk").unwrap();
                    name.starts_with(&format!("{stem}-")) || name.starts_with(&format!("{apk}-"))
                })
            })
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state(hash: char) -> DataState {
        DataState {
            schema_version: 1,
            manifest: FileDigest {
                size: 1,
                sha256: hash.to_string().repeat(64),
            },
            signers: [("com.google.android.gms".into(), vec!["a".repeat(64)])].into(),
        }
    }
    #[test]
    fn fresh_only_data_preserves_existing_users_and_rejects_implicit_disable() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("system")).unwrap();
        fs::write(root.path().join("system/packages.xml"), "existing user").unwrap();
        assert!(reconcile_data(root.path(), Some(state('a'))).is_err());
        assert!(!root.path().join(STATE).exists());
        assert_eq!(
            fs::read_to_string(root.path().join("system/packages.xml")).unwrap(),
            "existing user"
        );
        let fresh = tempfile::tempdir().unwrap();
        reconcile_data(fresh.path(), Some(state('a'))).unwrap();
        reconcile_data(fresh.path(), Some(state('a'))).unwrap();
        assert!(reconcile_data(fresh.path(), None).is_err());
        let mut changed = state('b');
        changed.signers.clear();
        assert!(reconcile_data(fresh.path(), Some(changed)).is_err());
    }
    #[test]
    fn update_invalidates_only_google_parser_cache_without_following_links() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        reconcile_data(root.path(), Some(state('a'))).unwrap();
        let cache = root.path().join("system/package_cache/37");
        fs::create_dir_all(&cache).unwrap();
        for name in ["GmsCore-1", "Phonesky.apk-2", "Settings-3"] {
            fs::write(cache.join(name), "cache").unwrap();
        }
        fs::write(outside.path().join("GmsCore-1"), "outside").unwrap();
        symlink(outside.path(), cache.join("link")).unwrap();
        reconcile_data(root.path(), Some(state('b'))).unwrap();
        assert!(!cache.join("GmsCore-1").exists());
        assert!(!cache.join("Phonesky.apk-2").exists());
        assert!(cache.join("Settings-3").exists());
        assert!(outside.path().join("GmsCore-1").exists());
        let redirected = tempfile::tempdir().unwrap();
        reconcile_data(redirected.path(), Some(state('a'))).unwrap();
        symlink(outside.path(), redirected.path().join("system")).unwrap();
        assert!(reconcile_data(redirected.path(), Some(state('b'))).is_err());
    }
}
