//! Keep development-image properties out of the Android application view.
//!
//! Project the files before init loads its immutable property area, so Java
//! Build fields, getprop and direct build.prop reads agree. This is metadata
//! compatibility, not release signing or hardware-backed attestation.

use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

const DIRECTORY: &str = "android-properties";
// Include both current and legacy paths used by the pinned init property loader.
const FILES: &[&str] = &[
    "system/build.prop",
    "system/etc/prop.default",
    "system_ext/etc/build.prop",
    "system_ext/default.prop",
    "system_ext/build.prop",
    "vendor/default.prop",
    "vendor/build.prop",
    "vendor/etc/build.prop",
    "odm/etc/build.prop",
    "odm/default.prop",
    "odm/build.prop",
    "product/etc/build.prop",
    "product/default.prop",
    "product/build.prop",
    "system_dlkm/etc/build.prop",
    "vendor_dlkm/etc/build.prop",
    "odm_dlkm/etc/build.prop",
];
const PARTITIONS: &[&str] = &[
    "bootimage",
    "system",
    "system_ext",
    "vendor",
    "odm",
    "product",
    "system_dlkm",
    "vendor_dlkm",
    "odm_dlkm",
];
const DEFAULTS: &[(&str, &str)] = &[
    ("ro.debuggable", "0"),
    ("ro.force.debuggable", "0"),
    ("ro.secure", "1"),
    ("ro.adb.secure", "1"),
    ("ro.build.type", "user"),
    ("ro.build.tags", "release-keys"),
];

fn build_field(key: &str) -> Option<&str> {
    key.strip_prefix("ro.build.").or_else(|| {
        let (partition, field) = key.strip_prefix("ro.")?.split_once(".build.")?;
        PARTITIONS.contains(&partition).then_some(field)
    })
}

fn build_words(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| match word {
            "userdebug" | "eng" => "user".to_owned(),
            "test-keys" | "dev-keys" => "release-keys".to_owned(),
            _ => word
                .strip_suffix("-userdebug")
                .or_else(|| word.strip_suffix("-eng"))
                .map_or_else(|| word.to_owned(), |product| format!("{product}-user")),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn release_properties(original: &str) -> String {
    let mut output = String::with_capacity(original.len());
    for line in original.lines() {
        let replacement = line.split_once('=').and_then(|(key, value)| {
            let key = key.trim();
            let value = value.trim();
            let replacement =
                if let Some((_, replacement)) = DEFAULTS.iter().find(|(name, _)| *name == key) {
                    (*replacement).to_owned()
                } else {
                    match build_field(key)? {
                        "type" => "user".to_owned(),
                        "tags" => "release-keys".to_owned(),
                        "fingerprint" => {
                            let (identity, variant) = value.rsplit_once(':')?;
                            let (kind, _) = variant.split_once('/')?;
                            if !matches!(kind, "user" | "userdebug" | "eng") {
                                return None;
                            }
                            format!("{identity}:user/release-keys")
                        }
                        "flavor" | "description" | "display.id" => build_words(value),
                        _ => return None,
                    }
                };
            Some(format!("{key}={replacement}"))
        });
        output.push_str(replacement.as_deref().unwrap_or(line));
        output.push('\n');
    }
    output
}

/// Product properties load last. Enforce defaults even when absent from the base.
pub(crate) fn product_properties(original: &str) -> String {
    let mut output = String::new();
    for line in release_properties(original).lines() {
        if line
            .split_once('=')
            .is_some_and(|(key, _)| DEFAULTS.iter().any(|(name, _)| *name == key.trim()))
        {
            continue;
        }
        output.push_str(line);
        output.push('\n');
    }
    for (key, value) in DEFAULTS {
        output.push_str(&format!("{key}={value}\n"));
    }
    output
}

/// Prepare private overlays after package overrides, without modifying images.
pub(crate) fn prepare(root: &Path, runtime: &Path) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let directory = runtime.join(DIRECTORY);
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))?;
    let mut overlays = Vec::new();
    for (index, relative) in FILES.iter().enumerate() {
        let target = root.join(relative);
        let metadata = match fs::symlink_metadata(&target) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        // Do not follow an image's absolute symlink into the host filesystem.
        // Its canonical property file is projected separately.
        if metadata.file_type().is_symlink() {
            continue;
        }
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is not a regular Android property file",
                    target.display()
                ),
            ));
        }
        let original = fs::read_to_string(&target)?;
        let properties = release_properties(&original);
        if properties == original {
            continue;
        }
        let source = directory.join(format!("{index}.prop"));
        fs::write(&source, properties)?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644))?;
        overlays.push((source, target));
    }
    Ok(overlays)
}

pub(crate) fn cleanup(runtime: &Path) -> io::Result<()> {
    let directory = runtime.join(DIRECTORY);
    for index in 0..FILES.len() {
        match fs::remove_file(directory.join(format!("{index}.prop"))) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    match fs::remove_dir(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn normalizes_all_partition_metadata_without_changing_build_identity() {
        let mut input = String::from(
            "# pinned image\nro.build.version.sdk=37\nro.build.version.security_patch=2026-06-05\nro.boot.verifiedbootstate=green\nro.boot.flash.locked=1\nro.product.model=My eng phone\ncustom.build.tags=test-keys\n",
        );
        let unchanged = input.clone();
        for prefix in std::iter::once("ro.build".to_owned()).chain(
            PARTITIONS
                .iter()
                .map(|partition| format!("ro.{partition}.build")),
        ) {
            input.push_str(&format!("{prefix}.type=userdebug\n{prefix}.tags=test-keys\n{prefix}.fingerprint=Droidloom/product/device:17/BUILD/123:userdebug/test-keys\n"));
        }
        input.push_str("ro.build.flavor=droidloom-eng\nro.build.description=droidloom-userdebug 17 BUILD 123 test-keys\nro.build.display.id=droidloom-eng 17 BUILD dev-keys\nro.debuggable=1\nro.force.debuggable=1\nro.secure=0\nro.adb.secure=0\n");
        let result = release_properties(&input);
        for unchanged in unchanged.lines() {
            assert!(result.lines().any(|line| line == unchanged));
        }
        assert!(!result.contains("userdebug"));
        assert!(!result.contains("dev-keys"));
        assert_eq!(
            result.matches(":17/BUILD/123:user/release-keys").count(),
            PARTITIONS.len() + 1
        );
        assert!(result.contains("ro.build.description=droidloom-user 17 BUILD 123 release-keys\n"));
        assert!(result.contains("ro.build.display.id=droidloom-user 17 BUILD release-keys\n"));
        assert_eq!(release_properties(&result), result);
    }

    #[test]
    fn product_defaults_apply_without_adb_and_replace_duplicates() {
        let input = "# identity\nro.product.model=Droidloom\nro.debuggable=1\n ro.debuggable = 1\nro.secure=0\ntraced.relay_producer_port=\n";
        let result = product_properties(input);
        for (key, value) in DEFAULTS {
            let entries: Vec<_> = result
                .lines()
                .filter(|line| line.starts_with(&format!("{key}=")))
                .collect();
            assert_eq!(entries, [format!("{key}={value}")]);
        }
        assert!(result.contains("ro.product.model=Droidloom\n"));
        assert!(result.contains("traced.relay_producer_port=\n"));
        assert_eq!(product_properties(&result), result);
        assert!(product_properties("").contains("ro.debuggable=0\n"));
    }

    #[test]
    fn overlays_preserve_base_files_and_skip_absolute_links() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        let runtime = temporary.path().join("runtime");
        fs::create_dir_all(root.join("system")).unwrap();
        fs::create_dir_all(root.join("product/etc")).unwrap();
        fs::create_dir(&runtime).unwrap();
        let original = "ro.debuggable=1\nro.build.tags=test-keys\nro.product.model=Droidloom\n";
        fs::write(root.join("system/build.prop"), original).unwrap();
        fs::write(
            root.join("product/etc/build.prop"),
            "ro.product.model=Droidloom\n",
        )
        .unwrap();
        symlink("/etc/build.prop", root.join("product/build.prop")).unwrap();
        let overlays = prepare(&root, &runtime).unwrap();
        assert_eq!(overlays.len(), 1);
        let (source, target) = &overlays[0];
        assert_eq!(target, &root.join("system/build.prop"));
        assert_eq!(
            fs::read_to_string(source).unwrap(),
            release_properties(original)
        );
        assert_eq!(
            fs::metadata(source).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read_to_string(target).unwrap(), original);
        cleanup(&runtime).unwrap();
        cleanup(&runtime).unwrap();
        assert!(!runtime.join(DIRECTORY).exists());
    }
}
