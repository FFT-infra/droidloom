use crate::util::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::Path, process::Command};
pub const MANIFEST: &str = "droidloom-update.json";
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub mode: u32,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub build_id: String,
    pub architecture: String,
    pub input_abi: u16,
    pub source_identity: String,
    pub files: BTreeMap<String, Artifact>,
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
    for path in files(root)? {
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
fn inventory(root: &Path) -> Result<BTreeMap<String, Artifact>> {
    let mut map = BTreeMap::new();
    for path in files(root)? {
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .ok_or("non-UTF8 bundle path")?;
        if relative == MANIFEST {
            continue;
        }
        map.insert(
            relative.to_owned(),
            Artifact {
                mode: fs::metadata(path)?.permissions().mode() & 0o777,
            },
        );
    }
    Ok(map)
}
pub fn seal(root: &Path, source_identity: String) -> Result<String> {
    let (architecture, input_abi) = compatibility(root)?;
    let manifest = Manifest {
        schema: 2,
        build_id: format!(
            "{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
            std::process::id()
        ),
        architecture,
        input_abi,
        source_identity,
        files: inventory(root)?,
    };
    write(&root.join(MANIFEST), serde_json::to_vec_pretty(&manifest)?)?;
    Ok(manifest.build_id)
}
pub fn verify(root: &Path) -> Result<Manifest> {
    let m: Manifest = serde_json::from_slice(&fs::read(root.join(MANIFEST))?)?;
    if m.schema != 2
        || m.build_id.is_empty()
        || !m.build_id.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    {
        return fail("invalid bundle build record");
    }
    for (relative, artifact) in &m.files {
        let path = Path::new(relative);
        if path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return fail("invalid component path");
        }
        let metadata = fs::metadata(root.join(path))?;
        if !metadata.is_file() || metadata.len() == 0 {
            return fail(format!("missing or empty component: {relative}"));
        }
        if artifact.mode & 0o111 != 0 && metadata.permissions().mode() & 0o111 == 0 {
            return fail(format!("component is not executable: {relative}"));
        }
    }
    let (arch, abi) = compatibility(root)?;
    if arch != m.architecture || abi != m.input_abi {
        return fail("manifest disagrees with compiled artifacts");
    }
    Ok(m)
}
pub fn source_identity(repo: &Path) -> Result<String> {
    let revision =
        output(
            Command::new("git")
                .current_dir(repo)
                .args(["rev-parse", "--short", "HEAD"]),
        )?;
    let dirty = !output(
        Command::new("git")
            .current_dir(repo)
            .args(["status", "--porcelain"]),
    )?
    .is_empty();
    Ok(format!("{revision}{}", if dirty { "+local" } else { "" }))
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
mod artifact_tests {
    use super::*;
    fn fixture(root: &Path, composer: u16, java: u16) {
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
    fn refuses_bundle_without_minimal_system_apps() {
        for relative in ["ime/DroidloomHome.apk", "ime/home-setup", "systemui/SystemUI.apk"] {
            let d = tempfile::tempdir().unwrap();
            fixture(d.path(), 5, 5);
            fs::remove_file(d.path().join("usr/lib/droidloom/runtime").join(relative)).unwrap();
            assert!(seal(d.path(), "fixture".into()).is_err());
        }
    }
    #[test]
    fn refuses_nonexecutable_input_launcher_even_when_recorded_that_way() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 5, 5);
        seal(d.path(), "fixture".into()).unwrap();
        let launcher = d
            .path()
            .join("usr/lib/droidloom/runtime/ime/droidloom-input-bridge");
        mode(&launcher, 0o644).unwrap();
        assert!(verify(d.path()).is_err());
        // A manifest made from a bad installation must not legitimize the mode.
        assert!(seal(d.path(), "fixture".into()).is_err());
    }
    #[test]
    fn refuses_mixed_native_java_bundle() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 4, 5);
        assert!(seal(d.path(), "fixture".into()).is_err());
        assert!(!d.path().join(MANIFEST).exists());
    }
    #[test]
    fn checks_required_components_without_requiring_identical_bytes() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 5, 5);
        seal(d.path(), "fixture".into()).unwrap();
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
        verify(d.path()).unwrap();
        fs::remove_file(&file).unwrap();
        assert!(verify(d.path()).is_err());
        write(&file, &original).unwrap();
        write(&d.path().join("unexpected"), b"old component").unwrap();
        verify(d.path()).unwrap();
    }
    #[test]
    fn rejects_foreign_architecture_in_other_component() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), 5, 5);
        let mut elf = vec![0; 20];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[18] = 183;
        write(&d.path().join("wrong-library.so"), elf).unwrap();
        assert!(seal(d.path(), "fixture".into()).is_err());
    }
}
