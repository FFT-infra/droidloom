use crate::util::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

pub const MANIFEST: &str = "droidloom-update.json";
pub const PRESENTER: &str = "usr/bin/droidloom-wayland";
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_COMPONENTS: usize = 64;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: String,
    pub component: String,
    pub size: u64,
    pub sha256: String,
    pub mode: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BundleKind {
    Full,
    PresenterDerivative,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub name: String,
    pub source_commit: String,
    pub cargo_lock_sha256: Option<String>,
    pub source_lock_sha256: Option<String>,
    pub target: String,
    pub toolchain: String,
    pub build_profile: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub build_id: String,
    pub architecture: String,
    pub input_abi: u16,
    pub source_identity: String,
    pub kind: BundleKind,
    pub base_build_id: Option<String>,
    pub components: Vec<Component>,
    pub files: Vec<Artifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyManifest {
    pub schema: u32,
    pub build_id: String,
    pub architecture: String,
    pub input_abi: u16,
    pub source_identity: String,
    pub files: BTreeMap<String, LegacyArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyArtifact {
    pub mode: u32,
}

pub enum ManifestRecord {
    Legacy(LegacyManifest),
    VerifiedFormat(Manifest),
}

/// Build provenance is captured before compilation and rechecked before sealing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildProvenance {
    pub source_commit: String,
    pub cargo_lock_sha256: String,
    pub source_lock_sha256: String,
    pub toolchain: String,
    pub target: String,
}

impl BuildProvenance {
    pub fn capture(repo: &Path, architecture: &str) -> Result<Self> {
        let source_commit = clean_source_commit(repo)?;
        let result = Self {
            source_commit,
            cargo_lock_sha256: hash(&repo.join("Cargo.lock"))?,
            source_lock_sha256: hash(&repo.join("android/manifest/m2-sparse-source-lock.json"))?,
            toolchain: output(Command::new("rustc").arg("--version"))?,
            target: host_target(architecture)?.to_owned(),
        };
        Ok(result)
    }

    pub fn check_unchanged(&self, repo: &Path) -> Result<()> {
        if clean_source_commit(repo)? != self.source_commit
            || hash(&repo.join("Cargo.lock"))? != self.cargo_lock_sha256
            || hash(&repo.join("android/manifest/m2-sparse-source-lock.json"))? != self.source_lock_sha256
        {
            return fail("source or build locks changed during the build; refusing to seal");
        }
        Ok(())
    }

    fn components(&self) -> Vec<Component> {
        ["android", "presenter", "runtime"]
            .into_iter()
            .map(|name| Component {
                name: name.into(),
                source_commit: self.source_commit.clone(),
                cargo_lock_sha256: (name != "android").then(|| self.cargo_lock_sha256.clone()),
                source_lock_sha256: Some(self.source_lock_sha256.clone()),
                target: self.target.clone(),
                toolchain: self.toolchain.clone(),
                build_profile: "release".into(),
            })
            .collect()
    }
}

pub fn host_target(architecture: &str) -> Result<&'static str> {
    match architecture {
        "x86_64" => Ok("x86_64-unknown-linux-gnu"),
        "aarch64" => Ok("aarch64-unknown-linux-gnu"),
        _ => fail("unsupported bundle architecture"),
    }
}
pub fn input_abi(data: &[u8]) -> Result<u16> {
    let prefix = b"DROIDLOOM_INPUT_ABI=";
    let mut versions = std::collections::BTreeSet::new();
    for i in 0..data.len().saturating_sub(prefix.len()) {
        if data[i..].starts_with(prefix) {
            let tail = &data[i + prefix.len()..];
            let n = tail.iter().take_while(|b| b.is_ascii_digit()).count();
            if n > 0 && n <= 5 && tail.get(n) == Some(&b';') {
                versions.insert(std::str::from_utf8(&tail[..n])?.parse::<u16>()?);
            }
        }
    }
    if versions.len() != 1 {
        return fail(
            "missing or ambiguous compiled input ABI metadata; rebuild all Android components",
        );
    }
    Ok(*versions.first().unwrap())
}
pub fn dex(path: &Path) -> Result<Vec<u8>> {
    let entries = output(Command::new("unzip").args(["-Z1"]).arg(path))?;
    let mut result = Vec::new();
    for name in entries
        .lines()
        .filter(|n| n.starts_with("classes") && n.ends_with(".dex"))
    {
        let out = Command::new("unzip")
            .arg("-p")
            .arg(path)
            .arg(name)
            .output()?;
        if !out.status.success() {
            return fail("cannot extract compiled Android DEX");
        }
        result.extend(out.stdout);
    }
    if result.is_empty() {
        return fail(format!(
            "{} is not a dexed Android runtime JAR",
            path.display()
        ));
    }
    Ok(result)
}
fn architecture(data: &[u8]) -> Result<&'static str> {
    if data.len() < 20 || &data[..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return fail("expected little-endian ELF artifact");
    }
    match u16::from_le_bytes([data[18], data[19]]) {
        62 => Ok("x86_64"),
        183 => Ok("aarch64"),
        _ => fail("unsupported ELF architecture"),
    }
}
fn compatibility(root: &Path) -> Result<(String, u16)> {
    let runtime = root.join("usr/lib/droidloom/runtime");
    let home = dex(&runtime.join("ime/DroidloomHome.apk"))?;
    if !home.windows(b"Lcom/android/droidloom/home/HomeActivity;".len())
        .any(|w| w == b"Lcom/android/droidloom/home/HomeActivity;")
    {
        return fail("DroidloomHome.apk lacks compiled HOME activity");
    }
    if fs::metadata(runtime.join("ime/home-setup"))?.len() == 0 {
        return fail("missing HOME provisioning script");
    }
    let systemui = dex(&runtime.join("systemui/SystemUI.apk"))?;
    if !systemui.windows(b"DROIDLOOM_SYSTEMUI_ABI=1;".len())
        .any(|w| w == b"DROIDLOOM_SYSTEMUI_ABI=1;")
    {
        return fail("SystemUI.apk is not the Droidloom minimal implementation");
    }
    if !systemui.windows(b"DROIDLOOM_CLIPBOARD_ABI=1;".len())
        .any(|w| w == b"DROIDLOOM_CLIPBOARD_ABI=1;") {
        return fail("SystemUI.apk lacks the clipboard adapter");
    }
    if !systemui.windows(b"DROIDLOOM_NOTIFICATIONS_ABI=1;".len()).any(|w| w == b"DROIDLOOM_NOTIFICATIONS_ABI=1;") {
        return fail("SystemUI.apk lacks the notification adapter");
    }
    let launcher = runtime.join("ime/droidloom-input-bridge");
    if fs::metadata(&launcher)?.permissions().mode() & 0o111 == 0 {
        return fail(
            "Android input bridge launcher is not executable; input and resizing cannot start",
        );
    }
    let composer =
        fs::read(runtime.join("bin/android.hardware.graphics.composer3-service.droidloom"))?;
    let arch = architecture(&composer)?.to_string();
    let version = input_abi(&composer)?;
    let mut bridges = 0;
    for relative in [
        "framework/droidloom-input-bridge.jar",
        "ime/droidloom-input-bridge.jar",
    ] {
        if !runtime.join(relative).is_file() {
            return fail(format!("missing Android bridge copy: {relative}"));
        }
    }
    let services = dex(&runtime.join("framework/services.jar"))?;
    if !services
        .windows(b"ro.vendor.droidloom.surfaceflinger_tasks".len())
        .any(|w| w == b"ro.vendor.droidloom.surfaceflinger_tasks")
    {
        return fail("services.jar lacks compiled Droidloom navigation policy");
    }
    for path in payload_files(root)? {
        if path
            .file_name()
            .is_some_and(|n| n == "droidloom-input-bridge.jar")
        {
            let data = dex(&path)?;
            if !data.windows(b"DROIDLOOM_CLIPBOARD_RELAY_ABI=1;".len())
                .any(|w| w == b"DROIDLOOM_CLIPBOARD_RELAY_ABI=1;") {
                return fail("Android input bridge lacks the clipboard relay");
            }
            if !data.windows(b"DROIDLOOM_NOTIFICATIONS_RELAY_ABI=1;".len()).any(|w| w == b"DROIDLOOM_NOTIFICATIONS_RELAY_ABI=1;") {
                return fail("Android input bridge lacks the notification relay");
            }
            let receiver = input_abi(&data)?;
            if version != receiver {
                return fail(format!(
                    "Composer input ABI v{version} does not match {} v{receiver}",
                    path.display()
                ));
            }
            if !data
                .windows(b"Lcom/android/droidloom/catalog/ApplicationCatalog;".len())
                .any(|w| w == b"Lcom/android/droidloom/catalog/ApplicationCatalog;")
            {
                return fail("bridge is missing the application catalog class");
            }
            bridges += 1;
        }
        let mut f = fs::File::open(&path)?;
        use std::io::Read;
        let mut header = [0; 20];
        if f.read(&mut header)? == 20
            && &header[..4] == b"\x7fELF"
            && architecture(&header)? != arch
        {
            return fail(format!("wrong ELF architecture: {}", path.display()));
        }
    }
    if bridges == 0 {
        return fail("missing Android bridge");
    }
    Ok((arch, version))
}
pub fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
        && value.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

pub fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn valid_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn valid_build_id(value: &str) -> bool {
    value.len() <= 128
        && value.split_once('-').is_some_and(|(time, pid)| {
            !time.is_empty() && !pid.is_empty()
                && time.bytes().all(|c| c.is_ascii_digit())
                && pid.bytes().all(|c| c.is_ascii_digit())
        })
}

pub fn new_build_id() -> Result<String> {
    Ok(format!("{}-{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?.as_nanos(), std::process::id()))
}

pub fn executable_architecture(path: &Path) -> Result<&'static str> {
    let mut file = fs::File::open(path)?;
    let mut header = [0u8; 20];
    file.read_exact(&mut header)?;
    architecture(&header)
}

/// Keep traversal bounded before either hashing or privileged staging starts.
pub fn payload_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !fs::symlink_metadata(root)?.is_dir() {
        return fail("bundle root must be a directory, not a symlink");
    }
    let mut pending = vec![(root.to_owned(), 0usize)];
    let mut result = Vec::new();
    let mut entries = 0;
    let mut total = 0u64;
    while let Some((directory, depth)) = pending.pop() {
        if depth > 64 { return fail("bundle directory nesting exceeds limit"); }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            entries += 1;
            if entries > MAX_FILES * 2 { return fail("bundle entry count exceeds limit"); }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push((entry.path(), depth + 1));
            } else if kind.is_file() {
                let size = entry.metadata()?.len();
                total = total.checked_add(size).ok_or("bundle size overflow")?;
                if size > MAX_FILE_BYTES || total > MAX_TOTAL_BYTES || result.len() >= MAX_FILES {
                    return fail("bundle payload exceeds size or file-count limit");
                }
                result.push(entry.path());
            } else {
                return fail(format!("bundle symlinks and non-regular inputs are forbidden: {}", entry.path().display()));
            }
        }
    }
    result.sort();
    Ok(result)
}

fn component_for(path: &str) -> &'static str {
    if path == PRESENTER {
        "presenter"
    } else if path.starts_with("usr/lib/droidloom/runtime/") || path.starts_with("var/lib/droidloom/images/") {
        "android"
    } else {
        "runtime"
    }
}

pub fn inventory(root: &Path) -> Result<Vec<Artifact>> {
    let mut result = Vec::new();
    for path in payload_files(root)? {
        let relative = path.strip_prefix(root)?.to_str().ok_or("non-UTF8 bundle path")?;
        if relative == MANIFEST { continue; }
        if !valid_relative_path(relative) { return fail("invalid component path"); }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.mode() & 0o7000 != 0 || metadata.len() == 0 {
            return fail(format!("empty or privileged bundle component: {relative}"));
        }
        result.push(Artifact {
            path: relative.into(),
            component: component_for(relative).into(),
            size: metadata.len(),
            sha256: hash(&path)?,
            mode: metadata.mode() & 0o777,
        });
    }
    Ok(result)
}

pub fn validate_manifest(m: &Manifest) -> Result<()> {
    if m.schema != 3 || !valid_build_id(&m.build_id) || m.input_abi == 0
        || m.source_identity.is_empty() || m.source_identity.len() > 4096
        || m.source_identity.chars().any(char::is_control)
    {
        return fail("invalid schema-3 bundle header");
    }
    let target = host_target(&m.architecture)?;
    match (m.kind, m.base_build_id.as_deref()) {
        (BundleKind::Full, None) => {}
        (BundleKind::PresenterDerivative, Some(base)) if valid_build_id(base) && base != m.build_id => {}
        _ => return fail("invalid bundle base identity"),
    }
    if m.files.is_empty() || m.files.len() > MAX_FILES || m.components.is_empty() || m.components.len() > MAX_COMPONENTS {
        return fail("bundle record count exceeds limits");
    }
    let mut components = BTreeSet::new();
    for c in &m.components {
        if !matches!(c.name.as_str(), "android" | "presenter" | "runtime")
            || !components.insert(c.name.as_str()) || !valid_commit(&c.source_commit)
            || c.target != target || c.toolchain.is_empty() || c.toolchain.len() > 1024
            || c.toolchain.chars().any(char::is_control) || c.build_profile != "release"
            || c.cargo_lock_sha256.as_deref().is_some_and(|v| !valid_digest(v))
            || c.source_lock_sha256.as_deref().is_some_and(|v| !valid_digest(v))
            || (c.name == "android" && c.source_lock_sha256.is_none())
            || (c.name != "android" && c.cargo_lock_sha256.is_none())
        {
            return fail("invalid or duplicate component provenance");
        }
    }
    let mut previous: Option<&str> = None;
    let mut total = 0u64;
    for a in &m.files {
        if !valid_relative_path(&a.path) || a.path == MANIFEST
            || previous.is_some_and(|v| v >= a.path.as_str())
            || !components.contains(a.component.as_str()) || a.component != component_for(&a.path)
            || a.size == 0 || a.size > MAX_FILE_BYTES || !valid_digest(&a.sha256)
            || a.mode & !0o777 != 0
        {
            return fail(format!("invalid, duplicate or unordered file record: {}", a.path));
        }
        if (a.path.starts_with("usr/") || a.path.starts_with("etc/"))
            && (a.mode & 0o004 == 0 || a.mode & 0o022 != 0
                || (a.mode & 0o111 != 0 && a.mode & 0o001 == 0))
        {
            return fail(format!("runtime component has unsafe installation permissions: {} (mode {:o})", a.path, a.mode));
        }
        previous = Some(&a.path);
        total = total.checked_add(a.size).ok_or("bundle size overflow")?;
        if total > MAX_TOTAL_BYTES { return fail("bundle total size exceeds limit"); }
    }
    if !m.files.iter().any(|a| a.path == PRESENTER) {
        return fail("bundle lacks a recorded presenter");
    }
    Ok(())
}

pub fn verify_inventory(root: &Path, m: &Manifest) -> Result<()> {
    validate_manifest(m)?;
    let actual = payload_files(root)?;
    let actual: BTreeSet<_> = actual.iter()
        .map(|p| p.strip_prefix(root).map(Path::to_owned))
        .collect::<std::result::Result<_, _>>()?;
    let expected: BTreeSet<_> = m.files.iter().map(|a| PathBuf::from(&a.path))
        .chain(std::iter::once(PathBuf::from(MANIFEST))).collect();
    if actual != expected { return fail("bundle file inventory differs from its manifest"); }
    for a in &m.files {
        let path = root.join(&a.path);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() != a.size || metadata.mode() & 0o7777 != a.mode {
            return fail(format!("bundle size or permissions changed: {}", a.path));
        }
        if hash(&path)? != a.sha256 {
            return fail(format!("bundle content hash changed: {}", a.path));
        }
    }
    Ok(())
}

pub fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit { return fail("metadata input exceeds limit or is not a regular file"); }
    let mut bytes = Vec::new();
    file.by_ref().take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit { return fail("metadata input exceeds limit"); }
    Ok(bytes)
}

pub fn inspect(root: &Path) -> Result<ManifestRecord> {
    let bytes = read_bounded(&root.join(MANIFEST), MAX_MANIFEST_BYTES)?;
    let header: serde_json::Value = serde_json::from_slice(&bytes)?;
    match header.get("schema").and_then(serde_json::Value::as_u64) {
        Some(3) => {
            let m: Manifest = serde_json::from_slice(&bytes)?;
            validate_manifest(&m)?;
            Ok(ManifestRecord::VerifiedFormat(m))
        }
        Some(2) => {
            let m: LegacyManifest = serde_json::from_slice(&bytes)?;
            if m.schema != 2 || !valid_build_id(&m.build_id) || m.files.len() > MAX_FILES
                || m.files.iter().any(|(p, a)| !valid_relative_path(p) || a.mode & !0o777 != 0)
            {
                return fail("invalid legacy bundle record");
            }
            Ok(ManifestRecord::Legacy(m))
        }
        _ => fail("unsupported bundle manifest schema"),
    }
}

pub fn write_manifest(root: &Path, m: &Manifest) -> Result<()> {
    validate_manifest(m)?;
    let bytes = serde_json::to_vec_pretty(m)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES { return fail("bundle manifest exceeds size limit"); }
    durable_write(&root.join(MANIFEST), bytes)
}

pub fn seal(root: &Path, provenance: &BuildProvenance) -> Result<String> {
    let (architecture, input_abi) = compatibility(root)?;
    if provenance.target != host_target(&architecture)? { return fail("build provenance has the wrong target"); }
    let manifest = Manifest {
        schema: 3,
        build_id: new_build_id()?,
        architecture,
        input_abi,
        source_identity: provenance.source_commit.clone(),
        kind: BundleKind::Full,
        base_build_id: None,
        components: provenance.components(),
        files: inventory(root)?,
    };
    write_manifest(root, &manifest)?;
    Ok(manifest.build_id)
}

pub fn verify(root: &Path) -> Result<Manifest> {
    let root = root.canonicalize()?;
    let m = match inspect(&root)? {
        ManifestRecord::VerifiedFormat(m) => m,
        ManifestRecord::Legacy(m) => return fail(format!(
            "legacy schema 2 bundle {} ({}, input ABI {}, source {}) is not content-verified; build a clean schema-3 baseline before activation or derivation",
            m.build_id, m.architecture, m.input_abi, m.source_identity)),
    };
    verify_inventory(&root, &m)?;
    let (arch, abi) = compatibility(&root)?;
    if arch != m.architecture || abi != m.input_abi { return fail("manifest disagrees with compiled artifacts"); }
    Ok(m)
}

pub fn clean_source_commit(repo: &Path) -> Result<String> {
    let revision = output(Command::new("git").current_dir(repo).args(["rev-parse", "HEAD"]))?;
    if !valid_commit(&revision) || !output(Command::new("git").current_dir(repo).args(["status", "--porcelain", "--untracked-files=all"]))?.is_empty() {
        return fail("bundle provenance requires a clean Git checkout at a full commit");
    }
    Ok(revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mismatched_metadata_is_not_guessed() {
        assert!(input_abi(b"old binary").is_err());
        assert!(input_abi(b"DROIDLOOM_INPUT_ABI=4;DROIDLOOM_INPUT_ABI=5;").is_err());
        assert_eq!(input_abi(b"DROIDLOOM_INPUT_ABI=00005;").unwrap(), 5);
    }
    #[test]
    fn inventory_detects_modes_and_files() {
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("a"), b"one").unwrap();
        let a = inventory(d.path()).unwrap();
        mode(&d.path().join("a"), 0o700).unwrap();
        assert_ne!(a, inventory(d.path()).unwrap());
        write(&d.path().join("extra"), b"x").unwrap();
        assert_eq!(inventory(d.path()).unwrap().len(), 2);
    }
}

#[cfg(test)]
pub(crate) mod artifact_tests {
    use super::*;
    fn test_provenance(arch: &str) -> BuildProvenance {
        BuildProvenance {
            source_commit: "a".repeat(40),
            cargo_lock_sha256: "b".repeat(64),
            source_lock_sha256: "c".repeat(64),
            toolchain: "rustc test".into(),
            target: host_target(arch).unwrap().into(),
        }
    }
    pub(crate) fn fixture(root: &Path, composer: u16, java: u16) {
        let launcher = root.join("usr/lib/droidloom/runtime/ime/droidloom-input-bridge");
        write(&launcher, b"#!/system/bin/sh\nexec app_process\n").unwrap();
        mode(&launcher, 0o755).unwrap();
        let mut elf = vec![0; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[18] = 62;
        elf.extend(format!("DROIDLOOM_INPUT_ABI={composer};").bytes());
        write(&root.join("usr/lib/droidloom/runtime/bin/android.hardware.graphics.composer3-service.droidloom"),elf).unwrap();
        let mut presenter_elf = vec![0; 64];
        presenter_elf[..4].copy_from_slice(b"\x7fELF");
        presenter_elf[4] = 2;
        presenter_elf[5] = 1;
        presenter_elf[18] = 62;
        let presenter = root.join(PRESENTER);
        write(&presenter, presenter_elf).unwrap();
        mode(&presenter, 0o755).unwrap();
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join("classes.dex"),format!("dex\n039\0DROIDLOOM_INPUT_ABI={java};DROIDLOOM_SYSTEMUI_ABI=1;DROIDLOOM_CLIPBOARD_ABI=1;DROIDLOOM_CLIPBOARD_RELAY_ABI=1;DROIDLOOM_NOTIFICATIONS_ABI=1;DROIDLOOM_NOTIFICATIONS_RELAY_ABI=1;Lcom/android/droidloom/catalog/ApplicationCatalog;Lcom/android/droidloom/home/HomeActivity;ro.vendor.droidloom.surfaceflinger_tasks")).unwrap();
        let jar = root.join("usr/lib/droidloom/runtime/framework/droidloom-input-bridge.jar");
        fs::create_dir_all(jar.parent().unwrap()).unwrap();
        run(Command::new("zip")
            .current_dir(d.path())
            .arg("-q")
            .arg(&jar)
            .arg("classes.dex"))
        .unwrap();
        copy(
            &jar,
            &root.join("usr/lib/droidloom/runtime/ime/droidloom-input-bridge.jar"),
        )
        .unwrap();
        copy(
            &jar,
            &root.join("usr/lib/droidloom/runtime/framework/services.jar"),
        )
        .unwrap();
        copy(&jar, &root.join("usr/lib/droidloom/runtime/ime/DroidloomHome.apk")).unwrap();
        copy(&jar, &root.join("usr/lib/droidloom/runtime/systemui/SystemUI.apk")).unwrap();
        write(&root.join("usr/lib/droidloom/runtime/ime/home-setup"), b"#!/system/bin/sh\n").unwrap();
    }
    #[test]
    fn refuses_runtime_files_unreadable_by_installed_service_users() {
        for relative in [
            "usr/lib/environment.d/60-droidloom.conf",
            "usr/lib/droidloom/runtime/lib64/restricted.so",
        ] {
            let d = tempfile::tempdir().unwrap();
            fixture(d.path(), 5, 5);
            let file = d.path().join(relative);
            write(&file, b"permission-validation fixture").unwrap();
            mode(&file, 0o600).unwrap();
            assert!(seal(d.path(), &test_provenance("x86_64")).is_err(),
                "root-owned runtime data must be readable after installation: {relative}");
        }
    }

    #[test]
    fn refuses_unsafe_runtime_execution_permissions() {
        for permissions in [0o744, 0o775, 0o777] {
            let d = tempfile::tempdir().unwrap();
            fixture(d.path(), 5, 5);
            mode(&d.path().join(PRESENTER), permissions).unwrap();
            assert!(seal(d.path(), &test_provenance("x86_64")).is_err());
        }
    }

    #[test]
    fn refuses_bundle_without_minimal_system_apps() {
        let provenance = test_provenance("x86_64");
        for relative in ["ime/DroidloomHome.apk", "ime/home-setup", "systemui/SystemUI.apk"] {
            let d = tempfile::tempdir().unwrap();
            fixture(d.path(), 5, 5);
            fs::remove_file(d.path().join("usr/lib/droidloom/runtime").join(relative)).unwrap();
            assert!(seal(d.path(), &provenance).is_err());
        }
    }
    #[test]
    fn refuses_nonexecutable_input_launcher_even_when_recorded_that_way() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 5, 5);
        let provenance = test_provenance("x86_64");
        seal(d.path(), &provenance).unwrap();
        let launcher = d
            .path()
            .join("usr/lib/droidloom/runtime/ime/droidloom-input-bridge");
        mode(&launcher, 0o644).unwrap();
        assert!(verify(d.path()).is_err());
        // A manifest made from a bad installation must not legitimize the mode.
        assert!(seal(d.path(), &provenance).is_err());
    }
    #[test]
    fn refuses_mixed_native_java_bundle() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 4, 5);
        assert!(seal(d.path(), &test_provenance("x86_64")).is_err());
        assert!(!d.path().join(MANIFEST).exists());
    }
    #[test]
    fn verifies_exact_content_instead_of_compatibility_only() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 5, 5);
        seal(d.path(), &test_provenance("x86_64")).unwrap();
        verify(d.path()).unwrap();
        let file = d.path().join(
            "usr/lib/droidloom/runtime/bin/android.hardware.graphics.composer3-service.droidloom",
        );
        let original = fs::read(&file).unwrap();
        write(&file, b"corrupt").unwrap();
        assert!(verify(d.path()).is_err());
        let mut compatible = original.clone();
        compatible.extend_from_slice(b"different build metadata");
        write(&file, &compatible).unwrap();
        assert!(verify(d.path()).is_err(), "different bytes at the same path must fail schema 3");
        fs::remove_file(&file).unwrap();
        assert!(verify(d.path()).is_err());
        write(&file, &original).unwrap();
        write(&d.path().join("unexpected"), b"old component").unwrap();
        assert!(verify(d.path()).is_err(), "unrecorded payload files must fail the exact inventory");
        fs::remove_file(d.path().join("unexpected")).unwrap();
        verify(d.path()).unwrap();
    }
    #[test]
    fn rejects_same_mode_content_tampering() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 5, 5);
        seal(d.path(), &test_provenance("x86_64")).unwrap();
        let file = d.path().join("usr/lib/droidloom/runtime/ime/home-setup");
        let mut bytes = fs::read(&file).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        write(&file, &bytes).unwrap();
        assert!(verify(d.path()).is_err(), "same size and mode with different bytes must fail");
    }
    fn rejects_foreign_architecture_in_other_component() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 5, 5);
        let mut elf = vec![0; 20];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[18] = 183;
        write(&d.path().join("wrong-library.so"), elf).unwrap();
        assert!(seal(d.path(), &test_provenance("x86_64")).is_err());
    }
}
