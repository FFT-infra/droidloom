//! Verified extraction of an official architecture-matched Android CI system base.
//!
//! Android dynamic partitions need several platform tools to unpack. This
//! library pins their inputs and orchestrates them without mounting an image or
//! accepting the Cuttlefish kernel/vendor as Droidloom runtime content.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;
use thiserror::Error;

const MAX_LOCK_BYTES: u64 = 1024 * 1024;
const REUSABLE_PARTITIONS: [&str; 3] = ["system_a", "system_ext_a", "product_a"];

/// Relevant subset of `android/manifest/source-lock.json`.
#[derive(Clone, Debug, Deserialize)]
pub struct SourceLock {
    /// Official prebuilt system base.
    pub aosp_ci_base: CiBaseLock,
}

/// Immutable identity and expected hashes for an Android CI image.
#[derive(Clone, Debug, Deserialize)]
pub struct CiBaseLock {
    /// CI branch.
    pub branch: String,
    /// CI target.
    pub target: String,
    /// Numeric CI build.
    pub build_number: String,
    /// Android build ID.
    pub build_id: String,
    /// Android release string.
    pub android_release: String,
    /// Android SDK integer.
    pub sdk: u32,
    /// Security patch level.
    pub security_patch: String,
    /// Primary native application ABI reported by the system image.
    pub cpu_abi: String,
    /// Stable artifact-viewer URL, not an expiring signed object URL.
    pub artifact_page_url: String,
    /// Artifact filename.
    pub artifact_name: String,
    /// Exact archive size.
    pub artifact_size: u64,
    /// Exact archive SHA-256.
    pub artifact_sha256: String,
    /// Exact framework resource package used to link vendor runtime overlays.
    pub framework_res: FrameworkResourceLock,
    /// Logical partition hashes keyed by slot-qualified name.
    pub partitions: BTreeMap<String, String>,
}

/// One file extracted from the pinned framework partition for vendor linking.
#[derive(Clone, Debug, Deserialize)]
pub struct FrameworkResourceLock {
    /// Path below the mounted system-partition root.
    pub path: PathBuf,
    /// Exact file size.
    pub size: u64,
    /// Exact lowercase SHA-256.
    pub sha256: String,
}

/// Machine-readable provenance emitted beside prepared base partitions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedBaseManifest {
    /// Manifest schema revision.
    pub schema_version: u32,
    /// CI branch.
    pub branch: String,
    /// CI target.
    pub target: String,
    /// Numeric CI build.
    pub build_number: String,
    /// Android build ID verified inside `system/build.prop`.
    pub build_id: String,
    /// Android release verified inside `system/build.prop`.
    pub android_release: String,
    /// SDK verified inside `system/build.prop`.
    pub sdk: u32,
    /// Security patch verified inside `system/build.prop`.
    pub security_patch: String,
    /// Primary native application ABI verified inside `system/build.prop`.
    pub cpu_abi: String,
    /// Stable Android CI artifact page.
    pub source_url: String,
    /// Source bundle hash.
    pub source_sha256: String,
    /// Only the three allowed reusable partitions.
    pub artifacts: Vec<PreparedArtifact>,
    /// Cuttlefish-specific inputs deliberately not copied into the output.
    pub excluded_inputs: Vec<String>,
}

/// One verified logical partition in the prepared system base.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedArtifact {
    /// Mount role without slot suffix.
    pub role: String,
    /// Manifest-relative output path.
    pub path: PathBuf,
    /// Exact size in bytes.
    pub size: u64,
    /// Exact SHA-256.
    pub sha256: String,
    /// Base partitions are always immutable.
    pub read_only: bool,
}

/// Successful dynamic-library closure report for the Droidloom vendor tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ElfClosureReport {
    /// Report schema revision.
    pub schema_version: u32,
    /// Native ELF executables and libraries inspected below the vendor root.
    pub consumers: usize,
    /// Unique `DT_NEEDED` names observed.
    pub dependencies: Vec<String>,
    /// Unique provider filenames available in permitted vendor/base library trees.
    pub providers: usize,
}

/// Failure to verify or prepare a base image.
#[derive(Debug, Error)]
pub enum PrepareError {
    /// Filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// Operation context.
        context: String,
        /// Underlying error.
        source: io::Error,
    },
    /// Source lock JSON was malformed.
    #[error("invalid source lock {path}: {source}")]
    LockJson {
        /// Source-lock path.
        path: PathBuf,
        /// JSON failure.
        source: serde_json::Error,
    },
    /// A pinned checksum, size, or build property did not match.
    #[error("verification failed: {0}")]
    Verification(String),
    /// A required Android image tool failed.
    #[error("command {program} failed ({status}): {stderr}")]
    Command {
        /// Program name.
        program: String,
        /// Exit status rendering.
        status: String,
        /// Bounded diagnostic stderr.
        stderr: String,
    },
}

/// Read the selected CI base lock with a bounded input size.
///
/// # Errors
///
/// Returns an I/O or JSON error, or rejects an oversized lock file.
pub fn load_source_lock(path: &Path) -> Result<SourceLock, PrepareError> {
    let metadata =
        fs::metadata(path).map_err(|source| io_error("read source lock metadata", source))?;
    if metadata.len() > MAX_LOCK_BYTES {
        return Err(PrepareError::Verification(format!(
            "source lock is {} bytes; maximum is {MAX_LOCK_BYTES}",
            metadata.len()
        )));
    }
    let bytes = fs::read(path).map_err(|source| io_error("read source lock", source))?;
    serde_json::from_slice(&bytes).map_err(|source| PrepareError::LockJson {
        path: path.to_path_buf(),
        source,
    })
}

/// Verify an Android CI archive and prepare only system, system-ext, and product.
///
/// This invokes `unzip`, `simg2img`, `lpunpack`, and `dump.erofs`. It never
/// mounts an image. The destination must not already exist, and becomes visible
/// only after every checksum and build property passes.
///
/// # Errors
///
/// Returns without publishing `destination` when a tool or verification fails.
pub fn prepare_base(
    archive: &Path,
    destination: &Path,
    lock: &CiBaseLock,
) -> Result<PreparedBaseManifest, PrepareError> {
    if destination.exists() {
        return Err(PrepareError::Verification(format!(
            "destination {} already exists",
            destination.display()
        )));
    }
    let parent = destination.parent().ok_or_else(|| {
        PrepareError::Verification("destination requires a parent directory".into())
    })?;
    fs::create_dir_all(parent).map_err(|source| io_error("create destination parent", source))?;

    let archive_sha256 = verify_archive(archive, lock)?;

    let staging = Builder::new()
        .prefix(".droidloom-base-")
        .tempdir_in(parent)
        .map_err(|source| io_error("create staging directory", source))?;
    let staging_path = staging.path();
    let artifacts = extract_partitions(archive, staging_path, lock)?;
    verify_build_properties(&staging_path.join("images/system.img"), lock)?;
    let manifest = make_manifest(lock, archive_sha256, artifacts);
    let manifest_json = serde_json::to_vec_pretty(&manifest)
        .map_err(|source| PrepareError::Verification(format!("serialize manifest: {source}")))?;
    fs::write(staging_path.join("base-manifest.json"), manifest_json)
        .map_err(|source| io_error("write base manifest", source))?;

    let work = staging_path.join("work");
    fs::remove_file(work.join("super.raw.img"))
        .map_err(|source| io_error("remove temporary raw super", source))?;
    fs::remove_file(work.join("super.img"))
        .map_err(|source| io_error("remove temporary sparse super", source))?;
    fs::remove_file(work.join("android-info.txt"))
        .map_err(|source| io_error("remove temporary android-info", source))?;
    fs::remove_dir(work).map_err(|source| io_error("remove empty work directory", source))?;

    fs::rename(staging_path, destination)
        .map_err(|source| io_error("publish prepared base atomically", source))?;
    Ok(manifest)
}

/// Verify that every `DT_NEEDED` entry in the vendor tree resolves by filename
/// in the vendor, system, system-ext, or product 64-bit library trees.
///
/// This is a deliberately stricter pre-boot check than merely inspecting the
/// handful of Droidloom executables: Mesa, allocator/mapper, and their complete
/// transitive vendor library set are all consumers.
///
/// # Errors
///
/// Returns an I/O/tool error, or a verification error listing unresolved
/// consumer/dependency pairs.
pub fn verify_elf_closure(
    vendor_root: &Path,
    system_root: &Path,
    system_ext_root: &Path,
    product_root: &Path,
) -> Result<ElfClosureReport, PrepareError> {
    let library_roots = [
        vendor_root.join("lib64"),
        system_root.join("lib64"),
        system_ext_root.join("lib64"),
        product_root.join("lib64"),
    ];
    let mut provider_names = BTreeSet::new();
    for root in &library_roots {
        collect_provider_names(root, &mut provider_names)?;
    }

    let mut consumers = Vec::new();
    collect_elf_files(&vendor_root.join("bin"), &mut consumers)?;
    collect_elf_files(&vendor_root.join("lib64"), &mut consumers)?;
    consumers.sort();
    consumers.dedup();

    let mut dependencies = BTreeSet::new();
    let mut missing = Vec::new();
    for consumer in &consumers {
        for needed in read_needed(consumer)? {
            dependencies.insert(needed.clone());
            if !provider_names.contains(&needed) {
                missing.push(format!("{} -> {needed}", consumer.display()));
            }
        }
    }
    if !missing.is_empty() {
        return Err(PrepareError::Verification(format!(
            "unresolved vendor ELF dependencies: {}",
            missing.join(", ")
        )));
    }
    Ok(ElfClosureReport {
        schema_version: 1,
        consumers: consumers.len(),
        dependencies: dependencies.into_iter().collect(),
        providers: provider_names.len(),
    })
}

fn collect_provider_names(
    root: &Path,
    providers: &mut BTreeSet<String>,
) -> Result<(), PrepareError> {
    if !root.exists() {
        return Ok(());
    }
    walk_regular_files(root, &mut |path| {
        if let Some(name) = path.file_name().and_then(OsStr::to_str) {
            providers.insert(name.to_owned());
        }
        Ok(())
    })
}

fn collect_elf_files(root: &Path, output: &mut Vec<PathBuf>) -> Result<(), PrepareError> {
    if !root.exists() {
        return Ok(());
    }
    walk_regular_files(root, &mut |path| {
        if starts_with(path, b"\x7fELF")? {
            output.push(path.to_path_buf());
        }
        Ok(())
    })
}

fn walk_regular_files(
    root: &Path,
    visitor: &mut impl FnMut(&Path) -> Result<(), PrepareError>,
) -> Result<(), PrepareError> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|source| io_error("read ELF closure directory", source))?;
        for entry in entries {
            let entry = entry.map_err(|source| io_error("read ELF closure entry", source))?;
            let file_type = entry
                .file_type()
                .map_err(|source| io_error("stat ELF closure entry", source))?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                visitor(&entry.path())?;
            }
        }
    }
    Ok(())
}

fn starts_with(path: &Path, expected: &[u8]) -> Result<bool, PrepareError> {
    let mut file = File::open(path).map_err(|source| io_error("open ELF candidate", source))?;
    let mut actual = vec![0; expected.len()];
    match file.read_exact(&mut actual) {
        Ok(()) => Ok(actual == expected),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(source) => Err(io_error("read ELF candidate", source)),
    }
}

fn read_needed(path: &Path) -> Result<Vec<String>, PrepareError> {
    let output = run_output("readelf", &[OsStr::new("-d"), path.as_os_str()])?;
    let stdout = String::from_utf8(output.stdout).map_err(|error| {
        PrepareError::Verification(format!(
            "readelf output for {} is not UTF-8: {error}",
            path.display()
        ))
    })?;
    Ok(stdout
        .lines()
        .filter(|line| line.contains("(NEEDED)"))
        .filter_map(|line| line.split_once("Shared library: ["))
        .filter_map(|(_, suffix)| suffix.split_once(']'))
        .map(|(name, _)| name.to_owned())
        .collect())
}

fn verify_archive(archive: &Path, lock: &CiBaseLock) -> Result<String, PrepareError> {
    let metadata = fs::metadata(archive).map_err(|source| io_error("stat archive", source))?;
    if metadata.len() != lock.artifact_size {
        return Err(PrepareError::Verification(format!(
            "archive size {} does not match pinned {}",
            metadata.len(),
            lock.artifact_size
        )));
    }
    let sha256 = sha256_file(archive)?;
    verify_equal("archive SHA-256", &lock.artifact_sha256, &sha256)?;
    Ok(sha256)
}

fn extract_partitions(
    archive: &Path,
    staging: &Path,
    lock: &CiBaseLock,
) -> Result<Vec<PreparedArtifact>, PrepareError> {
    let work = staging.join("work");
    let images = staging.join("images");
    fs::create_dir(&work).map_err(|source| io_error("create work directory", source))?;
    fs::create_dir(&images).map_err(|source| io_error("create image directory", source))?;
    run(
        "unzip",
        &[
            OsStr::new("-j"),
            archive.as_os_str(),
            OsStr::new("super.img"),
            OsStr::new("android-info.txt"),
            OsStr::new("-d"),
            work.as_os_str(),
        ],
    )?;
    let sparse_super = work.join("super.img");
    let raw_super = work.join("super.raw.img");
    run(
        "simg2img",
        &[sparse_super.as_os_str(), raw_super.as_os_str()],
    )?;
    let mut args: Vec<&OsStr> = Vec::new();
    for partition in REUSABLE_PARTITIONS {
        args.extend([OsStr::new("-p"), OsStr::new(partition)]);
    }
    args.extend([raw_super.as_os_str(), images.as_os_str()]);
    run("lpunpack", &args)?;
    verify_extracted_partitions(&images, lock)
}

fn verify_extracted_partitions(
    images: &Path,
    lock: &CiBaseLock,
) -> Result<Vec<PreparedArtifact>, PrepareError> {
    REUSABLE_PARTITIONS
        .into_iter()
        .map(|partition| {
            let slot_path = images.join(format!("{partition}.img"));
            let role = partition.trim_end_matches("_a");
            let final_name = format!("{role}.img");
            let final_path = images.join(&final_name);
            let actual_hash = sha256_file(&slot_path)?;
            let expected_hash = lock.partitions.get(partition).ok_or_else(|| {
                PrepareError::Verification(format!("source lock lacks {partition} hash"))
            })?;
            verify_equal(&format!("{partition} SHA-256"), expected_hash, &actual_hash)?;
            fs::rename(&slot_path, &final_path)
                .map_err(|source| io_error(&format!("rename {partition}"), source))?;
            Ok(PreparedArtifact {
                role: role.to_owned(),
                path: PathBuf::from("images").join(final_name),
                size: fs::metadata(&final_path)
                    .map_err(|source| io_error("stat prepared partition", source))?
                    .len(),
                sha256: actual_hash,
                read_only: true,
            })
        })
        .collect()
}

fn verify_build_properties(system_image: &Path, lock: &CiBaseLock) -> Result<(), PrepareError> {
    let output = run_output(
        "dump.erofs",
        &[
            OsStr::new("--cat"),
            OsStr::new("--path=/system/build.prop"),
            system_image.as_os_str(),
        ],
    )?;
    let build_prop = String::from_utf8(output.stdout).map_err(|error| {
        PrepareError::Verification(format!("system build.prop is not UTF-8: {error}"))
    })?;
    let properties = parse_properties(&build_prop);
    for (name, expected) in [
        ("ro.build.id", lock.build_id.as_str()),
        ("ro.build.version.incremental", lock.build_number.as_str()),
        ("ro.build.version.release", lock.android_release.as_str()),
        (
            "ro.build.version.security_patch",
            lock.security_patch.as_str(),
        ),
        ("ro.product.cpu.abi", lock.cpu_abi.as_str()),
    ] {
        verify_property(&properties, name, expected)?;
    }
    verify_property(&properties, "ro.build.version.sdk", &lock.sdk.to_string())
}

fn make_manifest(
    lock: &CiBaseLock,
    source_sha256: String,
    artifacts: Vec<PreparedArtifact>,
) -> PreparedBaseManifest {
    PreparedBaseManifest {
        schema_version: 1,
        branch: lock.branch.clone(),
        target: lock.target.clone(),
        build_number: lock.build_number.clone(),
        build_id: lock.build_id.clone(),
        android_release: lock.android_release.clone(),
        sdk: lock.sdk,
        security_patch: lock.security_patch.clone(),
        cpu_abi: lock.cpu_abi.clone(),
        source_url: lock.artifact_page_url.clone(),
        source_sha256,
        artifacts,
        excluded_inputs: [
            "boot",
            "init_boot",
            "vendor_boot",
            "vendor",
            "odm",
            "vendor_dlkm",
            "odm_dlkm",
            "cuttlefish_example_custom",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    }
}

fn verify_property(
    properties: &BTreeMap<String, String>,
    name: &str,
    expected: &str,
) -> Result<(), PrepareError> {
    let actual = properties
        .get(name)
        .ok_or_else(|| PrepareError::Verification(format!("build.prop lacks {name}")))?;
    verify_equal(name, expected, actual)
}

fn verify_equal(label: &str, expected: &str, actual: &str) -> Result<(), PrepareError> {
    if expected == actual {
        Ok(())
    } else {
        Err(PrepareError::Verification(format!(
            "{label} is {actual:?}, expected {expected:?}"
        )))
    }
}

fn parse_properties(input: &str) -> BTreeMap<String, String> {
    input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

fn sha256_file(path: &Path) -> Result<String, PrepareError> {
    let file = File::open(path).map_err(|source| io_error("open file for SHA-256", source))?;
    let mut reader = BufReader::with_capacity(8 * 1024 * 1024, file);
    let mut hash = Sha256::new();
    io::copy(&mut reader, &mut hash).map_err(|source| io_error("read file for SHA-256", source))?;
    Ok(hex::encode(hash.finalize()))
}

fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<(), PrepareError> {
    run_output(program, args).map(drop)
}

fn run_output(program: &str, args: &[&std::ffi::OsStr]) -> Result<Output, PrepareError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|source| io_error(&format!("execute {program}"), source))?;
    if output.status.success() {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(PrepareError::Command {
            program: program.to_owned(),
            status: output.status.to_string(),
            stderr: stderr.chars().take(4096).collect(),
        })
    }
}

fn io_error(context: &str, source: io::Error) -> PrepareError {
    PrepareError::Io {
        context: context.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn property_parser_ignores_comments_and_uses_last_assignment() {
        let properties = parse_properties("# generated\nro.build.id=old\n\nro.build.id=new\n");
        assert_eq!(
            properties.get("ro.build.id").map(String::as_str),
            Some("new")
        );
    }

    #[test]
    fn sha256_is_streamed_and_lowercase() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"droidloom").unwrap();
        assert_eq!(
            sha256_file(file.path()).unwrap(),
            "3d577e95401187c18da29f307b8a6d0be157120c8746206c52e4e98ad4324d3b"
        );
    }

    #[test]
    fn destination_must_not_exist_before_tool_execution() {
        let output = tempfile::tempdir().unwrap();
        let lock = CiBaseLock {
            branch: "branch".into(),
            target: "target".into(),
            build_number: "1".into(),
            build_id: "ID".into(),
            android_release: "17".into(),
            sdk: 37,
            security_patch: "2026-06-05".into(),
            cpu_abi: "arm64-v8a".into(),
            artifact_page_url: "https://ci.android.com/".into(),
            artifact_name: "image.zip".into(),
            artifact_size: 0,
            artifact_sha256: String::new(),
            framework_res: FrameworkResourceLock {
                path: "system/framework/framework-res.apk".into(),
                size: 0,
                sha256: String::new(),
            },
            partitions: BTreeMap::new(),
        };
        let error = prepare_base(Path::new("missing"), output.path(), &lock).unwrap_err();
        assert!(matches!(error, PrepareError::Verification(_)));
    }
}
