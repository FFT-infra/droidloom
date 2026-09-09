//! Optional Google-app images are bound to the exact base partitions they extend.
//!
//! This manifest records locally imported proprietary inputs. A checksum proves
//! identity, not permission to redistribute the payload or Google certification.

use std::{collections::BTreeMap, fs, io::Read, path::Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Architecture;

/// Current Google-app integration policy and manifest revision.
pub const SCHEMA_VERSION: u32 = 1;
/// First supported Android API level.
pub const SDK: u32 = 37;
/// Baseline partitions that must match, including the unmodified framework.
pub const BASE_ROLES: [&str; 3] = ["system", "system_ext", "product"];
/// Add-on partitions selected before Android init starts.
pub const ADDON_ROLES: [&str; 2] = ["system_ext", "product"];
/// Default package-owned add-on directory; activation is separately explicit.
pub const PACKAGE_DIRECTORY: &str = "/usr/lib/droidloom/addons/gapps";

/// Exact identity of an image or APK file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDigest {
    /// Bytes in the file.
    pub size: u64,
    /// Lowercase SHA-256 digest.
    pub sha256: String,
}

impl FileDigest {
    /// Hash one regular file without loading it into memory.
    ///
    /// # Errors
    /// Returns an error for a missing/non-regular file or an I/O failure.
    pub fn read(path: &Path) -> Result<Self, std::io::Error> {
        let mut file = fs::File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(std::io::Error::other(format!(
                "not a regular file: {}",
                path.display()
            )));
        }
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 131_072];
        let mut size = 0;
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            size += count as u64;
        }
        if size != meta.len() {
            return Err(std::io::Error::other("file size changed while hashing"));
        }
        Ok(Self {
            size,
            sha256: hex::encode(hash.finalize()),
        })
    }

    /// Check the bounded digest shape.
    pub fn is_valid(&self) -> bool {
        self.size > 0 && is_sha256(&self.sha256)
    }
}

/// Check a canonical SHA-256 string.
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One unmodified, signature-verified APK in its Android partition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Apk {
    /// Android application ID.
    pub package: String,
    /// Relative path including the partition name.
    pub path: String,
    /// Numeric Android version code.
    pub version_code: u64,
    /// API requirement read from the APK manifest.
    pub min_sdk: u32,
    /// Native ABIs advertised by the APK; empty for Java-only packages.
    pub native_abis: Vec<String>,
    /// Certificate digests returned by apksigner after successful verification.
    pub signer_sha256: Vec<String>,
    /// Digest of the APK bytes, which are never resigned.
    pub digest: FileDigest,
}

/// Relative paths and identities of the supported core applications.
pub const CORE_APPS: [(&str, &str); 3] = [
    (
        "com.google.android.gsf",
        "system_ext/priv-app/GoogleServicesFramework/GoogleServicesFramework.apk",
    ),
    (
        "com.google.android.gms",
        "product/priv-app/GmsCore/GmsCore.apk",
    ),
    (
        "com.android.vending",
        "product/priv-app/Phonesky/Phonesky.apk",
    ),
];
/// Optional synchronization adapters imported only when explicitly selected.
pub const SYNC_APPS: [(&str, &str); 2] = [
    (
        "com.google.android.syncadapters.calendar",
        "product/app/GoogleCalendarSyncAdapter/GoogleCalendarSyncAdapter.apk",
    ),
    (
        "com.google.android.syncadapters.contacts",
        "product/app/GoogleContactsSyncAdapter/GoogleContactsSyncAdapter.apk",
    ),
];

/// Provenance and compatibility contract for a complete optional image pair.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Must equal [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Guest architecture, independent of the machine that builds the package.
    pub architecture: Architecture,
    /// Android API level.
    pub sdk: u32,
    /// `LiteGapps` version recorded in `module.prop`.
    pub upstream_version: String,
    /// Original, locally supplied ZIP identity.
    pub archive: FileDigest,
    /// Exact system, `system_ext` and product inputs, keyed by role.
    pub base_images: BTreeMap<String, FileDigest>,
    /// Derived `system_ext` and product outputs, keyed by role.
    pub images: BTreeMap<String, FileDigest>,
    /// Imported applications and verified signers.
    pub apks: Vec<Apk>,
    /// Whether the two synchronization adapters were selected.
    pub sync_adapters: bool,
}

impl Manifest {
    /// Parse a size-bounded manifest and enforce the current policy.
    ///
    /// # Errors
    /// Rejects malformed, oversized or incompatible documents.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut bytes = Vec::new();
        fs::File::open(path)?.take(65_537).read_to_end(&mut bytes)?;
        if bytes.len() > 65_536 {
            return Err("GApps manifest exceeds 64 KiB".into());
        }
        let manifest: Self = serde_json::from_slice(&bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate exact roles, identities, APK paths and architecture requirements.
    ///
    /// # Errors
    /// Returns the first incompatible contract value.
    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        if self.schema_version != SCHEMA_VERSION || self.sdk != SDK {
            return Err("unsupported GApps manifest or Android SDK".into());
        }
        if self.upstream_version.is_empty()
            || self.upstream_version.len() > 64
            || !self.archive.is_valid()
        {
            return Err("invalid GApps upstream identity".into());
        }
        for (images, roles) in [
            (&self.base_images, BASE_ROLES.as_slice()),
            (&self.images, ADDON_ROLES.as_slice()),
        ] {
            if images.len() != roles.len()
                || roles
                    .iter()
                    .any(|role| !images.get(*role).is_some_and(FileDigest::is_valid))
            {
                return Err(
                    "GApps manifest must contain exactly the required image roles and hashes"
                        .into(),
                );
            }
        }
        let expected: BTreeMap<_, _> = CORE_APPS
            .into_iter()
            .chain(SYNC_APPS.into_iter().filter(|_| self.sync_adapters))
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        let abi = match self.architecture {
            Architecture::Aarch64 => "arm64-v8a",
            Architecture::X86_64 => "x86_64",
        };
        for apk in &self.apks {
            if expected.get(apk.package.as_str()) != Some(&apk.path.as_str())
                || !seen.insert(apk.package.as_str())
                || apk.min_sdk == 0
                || apk.min_sdk > SDK
                || apk.version_code == 0
                || !apk.digest.is_valid()
                || apk.signer_sha256.is_empty()
                || apk.signer_sha256.len() > 8
                || apk.signer_sha256.iter().any(|s| !is_sha256(s))
                || (!apk.native_abis.is_empty() && !apk.native_abis.iter().any(|s| s == abi))
            {
                return Err(format!("incompatible or duplicate GApps APK: {}", apk.package).into());
            }
        }
        if seen.len() != expected.len() {
            return Err("GApps manifest is missing selected applications".into());
        }
        Ok(())
    }

    /// Verify both the installed base and derived images before activation.
    ///
    /// Paths are derived from fixed roles, never supplied by the manifest.
    /// # Errors
    /// Rejects any wrong architecture, missing image or digest mismatch.
    pub fn verify_images(
        &self,
        base: &Path,
        addon: &Path,
        architecture: Architecture,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.validate()?;
        if self.architecture != architecture {
            return Err("GApps architecture does not match the Android runtime".into());
        }
        for (directory, images) in [(base, &self.base_images), (addon, &self.images)] {
            for (role, expected) in images {
                let path = directory.join("images").join(format!("{role}.img"));
                if FileDigest::read(&path)? != *expected {
                    return Err(format!("GApps image identity mismatch: {}; rebuild the add-on for the installed base", path.display()).into());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(root: &Path) -> Manifest {
        let base = root.join("base/images");
        let addon = root.join("addon/images");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&addon).unwrap();
        let mut manifest = Manifest {
            schema_version: 1,
            architecture: Architecture::Aarch64,
            sdk: SDK,
            upstream_version: "v4.9".into(),
            archive: FileDigest {
                size: 1,
                sha256: "a".repeat(64),
            },
            base_images: BTreeMap::new(),
            images: BTreeMap::new(),
            sync_adapters: false,
            apks: CORE_APPS
                .into_iter()
                .map(|(package, path)| Apk {
                    package: package.into(),
                    path: path.into(),
                    version_code: 1,
                    min_sdk: 28,
                    native_abis: vec![],
                    signer_sha256: vec!["b".repeat(64)],
                    digest: FileDigest {
                        size: 1,
                        sha256: "c".repeat(64),
                    },
                })
                .collect(),
        };
        for (directory, roles, map) in [
            (&base, BASE_ROLES.as_slice(), &mut manifest.base_images),
            (&addon, ADDON_ROLES.as_slice(), &mut manifest.images),
        ] {
            for role in roles {
                let path = directory.join(format!("{role}.img"));
                fs::write(&path, role).unwrap();
                map.insert((*role).into(), FileDigest::read(&path).unwrap());
            }
        }
        manifest
    }
    #[test]
    fn rejects_changed_base_output_architecture_and_manifest_paths() {
        let root = tempfile::tempdir().unwrap();
        let manifest = fixture(root.path());
        let base = root.path().join("base");
        let addon = root.path().join("addon");
        manifest
            .verify_images(&base, &addon, Architecture::Aarch64)
            .unwrap();
        assert!(
            manifest
                .verify_images(&base, &addon, Architecture::X86_64)
                .is_err()
        );
        fs::write(base.join("images/system.img"), "changed").unwrap();
        assert!(
            manifest
                .verify_images(&base, &addon, Architecture::Aarch64)
                .is_err()
        );
        let manifest = fixture(root.path());
        fs::write(addon.join("images/product.img"), "changed").unwrap();
        assert!(
            manifest
                .verify_images(&base, &addon, Architecture::Aarch64)
                .is_err()
        );
        let mut manifest = fixture(root.path());
        manifest
            .images
            .insert("../../escape".into(), manifest.archive.clone());
        assert!(manifest.validate().is_err());
        let mut manifest = fixture(root.path());
        manifest.apks[0].path = "../escape.apk".into();
        assert!(manifest.validate().is_err());
        let mut manifest = fixture(root.path());
        manifest.apks[1].native_abis = vec!["x86_64".into()];
        assert!(manifest.validate().is_err());
        let mut manifest = fixture(root.path());
        manifest.apks[1].signer_sha256.clear();
        assert!(manifest.validate().is_err());
    }
}
