//! Fedora Linux RPM staging and container build orchestration.
use super::{
    Architecture, Result, Version, build_workspace, compiler_cpus, copy_download_archives,
    copy_inputs, install_tree, podman, repository, require_user, run, run_logged, seed_sources,
};
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs,
    os::fd::AsRawFd,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

const FEDORA_BUILDER_IMAGE: &str = "localhost/droidloom-fedora-builder";

const HOST_PACKAGES: &[&str] = &[
    "droidloom-supervisor",
    "droidloom-wayland",
    "droidloom-applications",
    "droidloom-audio",
    "droidloom-gpu-server",
    "droidloom-devices",
    "droidloom-doctor",
    "droidloom-package-support",
];

const HOST_BINARY_PATHS: &[(&str, &str)] = &[
    ("droidloomd", "usr/bin/droidloomd"),
    ("droidloomctl", "usr/bin/droidloomctl"),
    ("droidloom-supervisor", "usr/bin/droidloom-supervisor"),
    ("droidloom-wayland", "usr/bin/droidloom-wayland"),
    ("droidloom-applications", "usr/bin/droidloom-applications"),
    ("droidloom-audio", "usr/bin/droidloom-audio"),
    ("droidloom-gpu-server", "usr/bin/droidloom-gpu-server"),
    ("droidloom-devices", "usr/bin/droidloom-devices"),
    ("droidloom-doctor", "usr/bin/droidloom-doctor"),
    (
        "droidloom-package-helper",
        "usr/lib/droidloom/droidloom-package-helper",
    ),
];

pub fn default_architecture() -> Architecture {
    match std::env::consts::ARCH {
        "aarch64" => Architecture::Aarch64,
        _ => Architecture::X86_64,
    }
}

const PROXY_VARS: &[&str] = &[
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
];

fn uses_loopback_proxy() -> bool {
    PROXY_VARS.iter().any(|var| {
        !var.eq_ignore_ascii_case("no_proxy")
            && std::env::var(var).is_ok_and(|value| {
                let lower = value.to_ascii_lowercase();
                lower.contains("127.0.0.1")
                    || lower.contains("localhost")
                    || lower.contains("[::1]")
            })
    })
}

fn fedora_output(version: &Version, repo: &Path) -> PathBuf {
    repo.join("dist/fedora")
        .join(version.directory())
        .join(&version.architecture)
}

#[allow(clippy::too_many_arguments)]
pub fn build_rpm(
    explicit: Option<PathBuf>,
    architecture: Option<Architecture>,
    sysroot: Option<PathBuf>,
    payload: Option<PathBuf>,
    requested: Option<usize>,
    cpuset: bool,
    clean: bool,
    source_cache: Option<PathBuf>,
    native_bridge_source: Option<PathBuf>,
) -> Result<()> {
    let started = Instant::now();
    require_user()?;
    let architecture = architecture.unwrap_or_else(default_architecture);
    if architecture.name() != std::env::consts::ARCH && payload.is_none() && sysroot.is_none() {
        return Err(format!(
            "building {} RPMs on {} requires --sysroot or a pre-staged --payload",
            architecture.name(),
            std::env::consts::ARCH
        )
        .into());
    }
    if architecture == Architecture::X86_64 && sysroot.is_some() {
        return Err("only aarch64 cross-builds take --sysroot".into());
    }
    if payload.is_some() && (clean || source_cache.is_some() || native_bridge_source.is_some()) {
        return Err("--payload cannot be combined with --clean, --source-cache, or --native-bridge-source".into());
    }
    if !Command::new("podman")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return Err("Podman is missing. Install it with: sudo dnf install podman".into());
    }
    if !Command::new("rsync")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return Err("rsync is missing. Install it with: sudo dnf install rsync".into());
    }

    let repo = repository(explicit)?;
    let version = Version::read(&repo, architecture)?;
    let cpus = compiler_cpus()?;
    let jobs = requested.unwrap_or(cpus.len()).min(cpus.len());
    if jobs == 0 {
        return Err("--jobs must be positive".into());
    }

    let work = std::env::var_os("DROIDLOOM_PACKAGE_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.join(".work/fedora"));
    if !work.is_absolute() {
        return Err("DROIDLOOM_PACKAGE_WORK must be an absolute path".into());
    }
    let _ = build_workspace(&repo)?;
    fs::create_dir_all(&work)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(work.join("build.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another Droidloom Fedora package build is already using this workspace".into());
    }

    let snapshot = work.join("source");
    copy_inputs(&repo, &snapshot)?;
    copy_download_archives(&repo, &snapshot)?;
    let cache = work.join("build");
    fs::create_dir_all(&cache)?;

    if let Some(seed) = source_cache {
        let seed = seed.canonicalize()?;
        let destination = cache.join("aosp-source");
        if destination.exists() {
            return Err("source cache already initialized; omit --source-cache".into());
        }
        if !seed.join(".droidloom-source-manifest.json").is_file() {
            return Err("--source-cache requires a sparse Droidloom AOSP source checkout".into());
        }
        let temporary = tempfile::Builder::new()
            .prefix("source-seed-")
            .tempdir_in(&cache)?;
        seed_sources(&seed, temporary.path(), Path::new(""))?;
        fs::rename(temporary.path(), &destination)?;
    }

    let output = fedora_output(&version, &repo);
    fs::create_dir_all(&output)?;
    if fs::read_dir(&output)?
        .any(|e| e.is_ok_and(|e| e.path().extension() == Some(OsStr::new("rpm"))))
    {
        return Err(format!(
            "{} already contains RPMs; increase packaging/arch/version.json release or move previous artifacts before rebuilding",
            output.display()
        )
        .into());
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let log_path = work.join(format!("build-{timestamp}.log"));
    let log = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&log_path)?;
    eprintln!(
        "Building Fedora RPMs {} for {} with {jobs} jobs; log: {}",
        version.directory(),
        version.architecture,
        log_path.display()
    );

    let host_network = uses_loopback_proxy();
    let mut build_image = podman(&work);
    build_image.arg("build");
    if host_network {
        build_image.arg("--network=host");
    }
    build_image
        .args(["--tag", FEDORA_BUILDER_IMAGE, "--file"])
        .arg(repo.join("packaging/fedora/Containerfile"))
        .arg(repo.join("packaging/fedora"));
    run_logged(&mut build_image, &log)?;
    run(podman(&work).args(["image", "prune", "--force"]))?;

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let mut container = podman(&work);
    container.args(["run", "--rm"]);
    if host_network {
        container.arg("--network=host");
    }
    container
        .args(["--security-opt", "label=disable", "--userns=keep-id", "--user"])
        .arg(format!("{uid}:{gid}"));
    for var in PROXY_VARS {
        if let Ok(value) = std::env::var(var) {
            container.arg("--env").arg(format!("{var}={value}"));
        }
    }
    if cpuset {
        container.arg("--cpuset-cpus").arg(
            cpus.iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    container
        .args([
            "--env",
            if cpuset {
                "DROIDLOOM_COMPILER_CPUS_RESERVED=1"
            } else {
                "DROIDLOOM_COMPILER_CPUS_RESERVED=0"
            },
            "--env",
            "CARGO_TARGET_DIR=/build/driver",
        ])
        .arg("--volume")
        .arg(format!("{}:/source:rw", snapshot.display()))
        .arg("--volume")
        .arg(format!("{}:/build:rw", cache.display()))
        .arg("--volume")
        .arg(format!("{}:/output:rw", output.display()));

    if let Some(sysroot) = sysroot {
        let sysroot = sysroot.canonicalize()?;
        container
            .arg("--volume")
            .arg(format!("{}:/sysroot:ro", sysroot.display()))
            .args(["--env", "DROIDLOOM_SYSROOT=/sysroot"]);
    }
    if let Some(payload) = payload {
        let payload = payload.canonicalize()?;
        if !payload.join("runtime").is_dir() || !payload.join("image").is_dir() {
            return Err("--payload must point to a package-stage directory containing runtime/ and image/".into());
        }
        container
            .arg("--volume")
            .arg(format!("{}:/payload:ro", payload.display()))
            .args(["--env", "DROIDLOOM_PREBUILT_PAYLOAD=/payload"]);
    }
    if let Some(checkout) = native_bridge_source {
        let checkout = checkout.canonicalize()?;
        if !checkout.join(".git").exists() {
            return Err("--native-bridge-source requires a Teto Git checkout".into());
        }
        container
            .args([
                "--env",
                "DROIDLOOM_NATIVE_BRIDGE_SOURCE=/native-bridge-source",
            ])
            .arg("--volume")
            .arg(format!("{}:/native-bridge-source:ro", checkout.display()));
    }

    container
        .arg(FEDORA_BUILDER_IMAGE)
        .args([
            "cargo",
            "run",
            "--locked",
            "--release",
            "-j",
            "1",
            "-p",
            "droidloom-package",
            "--",
            "in-fedora-container",
            "--architecture",
            architecture.name(),
            "--jobs",
        ])
        .arg(jobs.to_string());
    if clean {
        container.arg("--clean");
    }
    run_logged(&mut container, &log)?;

    let mut rpms = fs::read_dir(&output)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    rpms.retain(|p| p.extension() == Some(OsStr::new("rpm")));
    rpms.sort();
    if rpms.len() != 2 {
        return Err("Fedora build did not produce both runtime and image RPMs".into());
    }
    fs::write(
        output.join("build-summary.json"),
        serde_json::to_vec_pretty(&json!({
            "version": version,
            "elapsed_seconds": started.elapsed().as_secs(),
            "jobs": jobs,
            "packages": rpms.iter().map(|p| Ok(json!({
                "file": p.file_name().map(|n| n.to_string_lossy()),
                "bytes": fs::metadata(p)?.len(),
            }))).collect::<Result<Vec<_>>>()?
        }))?,
    )?;
    println!("Fedora RPMs ready in {}", output.display());
    if version.architecture == std::env::consts::ARCH {
        println!(
            "Install with:\nsudo dnf install {}",
            rpms.iter()
                .map(|p| super::quote_path(p))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    Ok(())
}

pub fn in_fedora_container(architecture: Architecture, jobs: usize, clean: bool) -> Result<()> {
    require_user()?;
    let repo = Path::new("/source");
    let work = Path::new("/build");
    let version = Version::read(repo, architecture)?;
    let staging = work.join("fedora-stage");
    super::remove_staging(&staging)?;
    fs::create_dir_all(&staging)?;

    let payload_stage = staging.join("payload");
    if let Some(prebuilt) = std::env::var_os("DROIDLOOM_PREBUILT_PAYLOAD") {
        install_tree(Path::new(&prebuilt), &payload_stage)?;
        if architecture.name() != std::env::consts::ARCH {
            return Err("cross-compiling Fedora host binaries with --payload is not supported; run natively on the target architecture".into());
        }
        compile_host_binaries(repo, work, jobs, &payload_stage.join("runtime"))?;
    } else {
        run(Command::new("cargo")
            .current_dir(repo)
            .env("CARGO_TARGET_DIR", work.join("cargo"))
            .args([
                "build",
                "--locked",
                "--release",
                "-p",
                "droidloom-update",
                "--jobs",
            ])
            .arg(jobs.to_string()))?;
        let mut command = Command::new(work.join("cargo/release/droidloom-update"));
        command
            .arg("--source")
            .arg(repo)
            .arg("package-stage")
            .arg("--work")
            .arg(work)
            .arg("--destination")
            .arg(&payload_stage)
            .arg("--jobs")
            .arg(jobs.to_string())
            .arg("--product")
            .arg(architecture.product());
        if let Some(sysroot) = std::env::var_os("DROIDLOOM_SYSROOT") {
            command.arg("--sysroot").arg(sysroot);
        }
        if clean {
            command.arg("--clean");
        }
        run(&mut command)?;
    }

    let stage_runtime = staging.join("stage-runtime");
    let stage_image = staging.join("stage-image");
    prepare_fedora_stage(repo, &payload_stage, &stage_runtime, &stage_image, architecture)?;

    let rpmbuild_top = staging.join("rpmbuild");
    for sub in ["BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"] {
        fs::create_dir_all(rpmbuild_top.join(sub))?;
    }
    let release_str = version.release.to_string();
    for spec in ["droidloom-image.spec", "droidloom-runtime.spec"] {
        let stage_dir = if spec == "droidloom-image.spec" {
            &stage_image
        } else {
            &stage_runtime
        };
        run(Command::new("rpmbuild")
            .arg("--define")
            .arg(format!("_topdir {}", rpmbuild_top.display()))
            .arg("--define")
            .arg(format!("stage_dir {}", stage_dir.display()))
            .arg("--define")
            .arg(format!("pkg_version {}", version.version))
            .arg("--define")
            .arg(format!("pkg_release {release_str}"))
            .args(["--target", architecture.name(), "-bb"])
            .arg(repo.join("packaging/fedora").join(spec)))?;
    }

    let rpms_arch = rpmbuild_top.join("RPMS").join(architecture.name());
    let mut built = Vec::new();
    for entry in fs::read_dir(&rpms_arch)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension() == Some(OsStr::new("rpm")) {
            let dest = Path::new("/output").join(entry.file_name());
            fs::copy(&path, &dest)?;
            built.push(dest);
        }
    }
    if built.len() != 2 {
        return Err("rpmbuild did not produce both runtime and image RPMs".into());
    }
    super::remove_staging(&staging)?;
    Ok(())
}

fn compile_host_binaries(repo: &Path, work: &Path, jobs: usize, runtime_dir: &Path) -> Result<()> {
    let mut build = Command::new("cargo");
    build
        .current_dir(repo)
        .env("CARGO_TARGET_DIR", work.join("cargo"))
        .args(["build", "--locked", "--release", "--jobs"])
        .arg(jobs.to_string());
    for package in HOST_PACKAGES {
        build.arg("-p").arg(package);
    }
    run(&mut build)?;

    let target_release = work.join("cargo/release");
    for (binary, relative) in HOST_BINARY_PATHS {
        let dest = runtime_dir.join(relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(target_release.join(binary), &dest)?;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Transform a generic `droidloom-update package-stage` tree into Fedora's
/// `stage-runtime` and `stage-image` directories for `rpmbuild`.
pub fn prepare_fedora_stage(
    repo: &Path,
    payload: &Path,
    stage_runtime: &Path,
    stage_image: &Path,
    architecture: Architecture,
) -> Result<()> {
    install_tree(&payload.join("runtime"), stage_runtime)?;
    install_tree(&payload.join("image"), stage_image)?;

    let alpm_hooks = stage_runtime.join("usr/share/libalpm");
    if alpm_hooks.exists() {
        fs::remove_dir_all(&alpm_hooks)?;
    }

    let selinux_dir = stage_runtime.join("usr/share/selinux/packages");
    fs::create_dir_all(&selinux_dir)?;
    let selinux_scratch = tempfile::Builder::new()
        .prefix(".selinux-")
        .tempdir_in(&selinux_dir)?;
    let mod_file = selinux_scratch.path().join("droidloom_fedora.mod");
    let pp_file = selinux_dir.join("droidloom_fedora.pp");
    run(Command::new("checkmodule")
        .args(["-M", "-m", "-o"])
        .arg(&mod_file)
        .arg(repo.join("packaging/fedora/selinux/droidloom_fedora.te")))?;
    run(Command::new("semodule_package")
        .arg("-o")
        .arg(&pp_file)
        .arg("-m")
        .arg(&mod_file))?;

    let fw_zones = stage_runtime.join("usr/lib/firewalld/zones");
    let fw_policies = stage_runtime.join("usr/lib/firewalld/policies");
    fs::create_dir_all(&fw_zones)?;
    fs::create_dir_all(&fw_policies)?;
    fs::copy(
        repo.join("packaging/fedora/firewalld/zones/droidloom.xml"),
        fw_zones.join("droidloom.xml"),
    )?;
    fs::copy(
        repo.join("packaging/fedora/firewalld/policies/droidloom-egress.xml"),
        fw_policies.join("droidloom-egress.xml"),
    )?;

    if architecture == Architecture::Aarch64 {
        stage_aarch64_adreno_overrides(stage_runtime, stage_image)?;
    }
    Ok(())
}

/// Adreno 830 (`msm_dpu`) UBWC mitigation for Fedora aarch64.
fn stage_aarch64_adreno_overrides(stage_runtime: &Path, stage_image: &Path) -> Result<()> {
    let runtime_root = stage_image.join("usr/lib/droidloom/runtime");
    let init_src = runtime_root.join("compat/init-classpath-first-pixels.rc");
    let prop_src = runtime_root.join("overrides/vendor/build.prop");
    let lib_dir = stage_runtime.join("usr/lib/droidloom");
    fs::create_dir_all(&lib_dir)?;

    let init_text = fs::read_to_string(&init_src)?;
    let init_patched = if init_text.contains("vendor.minigbm.debug") {
        init_text
    } else if let Some((before, after)) = init_text.split_once("export DROIDLOOM_CELL 1\n") {
        format!("{before}export DROIDLOOM_CELL 1\n    export vendor.minigbm.debug nocompression\n{after}")
    } else {
        format!("{init_text}\non early-init\n    export vendor.minigbm.debug nocompression\n")
    };
    fs::write(lib_dir.join("fedora-init.rc"), init_patched)?;

    let mut prop_text = fs::read_to_string(&prop_src)?;
    if !prop_text.lines().any(|l| l.starts_with("vendor.minigbm.debug=")) {
        if !prop_text.ends_with('\n') {
            prop_text.push('\n');
        }
        prop_text.push_str("vendor.minigbm.debug=nocompression\n");
    }
    fs::write(lib_dir.join("fedora-vendor-build.prop"), prop_text)?;

    let template_path = stage_runtime.join("usr/share/droidloom/cell-template.json");
    let mut template: Value = serde_json::from_slice(&fs::read(&template_path)?)?;
    if let Some(overrides) = template["android_file_overrides"].as_array_mut() {
        for entry in overrides {
            match entry["target"].as_str() {
                Some("/system/etc/init/hw/init.rc") => {
                    entry["source"] = json!("/usr/lib/droidloom/fedora-init.rc");
                }
                Some("/vendor/build.prop") => {
                    entry["source"] = json!("/usr/lib/droidloom/fedora-vendor-build.prop");
                }
                _ => {}
            }
        }
    }
    fs::write(&template_path, serde_json::to_vec_pretty(&template)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fedora_stage_prepares_x86_64_without_adreno_overrides_or_alpm_hooks() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let rt = payload.join("runtime");
        let img = payload.join("image");

        fs::create_dir_all(rt.join("usr/share/libalpm/hooks")).unwrap();
        fs::write(rt.join("usr/share/libalpm/hooks/test.hook"), b"hook").unwrap();
        fs::create_dir_all(rt.join("usr/share/droidloom")).unwrap();
        let template = serde_json::from_str::<Value>(include_str!(
            "../../../packaging/cell-spec-x86_64-u1000.json"
        ))
        .unwrap();
        fs::write(
            rt.join("usr/share/droidloom/cell-template.json"),
            serde_json::to_vec_pretty(&template).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(img.join("usr/lib/droidloom/runtime/compat")).unwrap();

        let stage_runtime = temp.path().join("stage-runtime");
        let stage_image = temp.path().join("stage-image");
        prepare_fedora_stage(&repo, &payload, &stage_runtime, &stage_image, Architecture::X86_64)
            .unwrap();

        assert!(!stage_runtime.join("usr/share/libalpm").exists());
        assert!(stage_runtime.join("usr/share/selinux/packages/droidloom_fedora.pp").is_file());
        assert!(stage_runtime.join("usr/lib/firewalld/zones/droidloom.xml").is_file());
        assert!(stage_runtime.join("usr/lib/firewalld/policies/droidloom-egress.xml").is_file());
        assert!(!stage_runtime.join("usr/lib/droidloom/fedora-init.rc").exists());
        assert!(!stage_runtime.join("usr/lib/droidloom/fedora-vendor-build.prop").exists());
    }

    #[test]
    fn fedora_stage_prepares_aarch64_with_adreno_nocompression_overrides() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        let rt = payload.join("runtime");
        let img = payload.join("image");

        fs::create_dir_all(rt.join("usr/share/droidloom")).unwrap();
        let template = serde_json::from_str::<Value>(include_str!(
            "../../../packaging/cell-spec-arm64.json"
        ))
        .unwrap();
        fs::write(
            rt.join("usr/share/droidloom/cell-template.json"),
            serde_json::to_vec_pretty(&template).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(img.join("usr/lib/droidloom/runtime/compat")).unwrap();
        fs::create_dir_all(img.join("usr/lib/droidloom/runtime/overrides/vendor")).unwrap();
        fs::write(
            img.join("usr/lib/droidloom/runtime/compat/init-classpath-first-pixels.rc"),
            "on early-init\n    export DROIDLOOM_CELL 1\n",
        )
        .unwrap();
        fs::write(
            img.join("usr/lib/droidloom/runtime/overrides/vendor/build.prop"),
            "ro.hardware.egl=mesa\n",
        )
        .unwrap();

        let stage_runtime = temp.path().join("stage-runtime");
        let stage_image = temp.path().join("stage-image");
        prepare_fedora_stage(&repo, &payload, &stage_runtime, &stage_image, Architecture::Aarch64)
            .unwrap();

        let init = fs::read_to_string(stage_runtime.join("usr/lib/droidloom/fedora-init.rc")).unwrap();
        assert!(init.contains("export vendor.minigbm.debug nocompression"));
        let prop = fs::read_to_string(stage_runtime.join("usr/lib/droidloom/fedora-vendor-build.prop")).unwrap();
        assert!(prop.contains("vendor.minigbm.debug=nocompression"));
        let updated: Value = serde_json::from_slice(
            &fs::read(stage_runtime.join("usr/share/droidloom/cell-template.json")).unwrap(),
        )
        .unwrap();
        let overrides = updated["android_file_overrides"].as_array().unwrap();
        assert!(overrides.iter().any(|e| {
            e["target"] == "/system/etc/init/hw/init.rc"
                && e["source"] == "/usr/lib/droidloom/fedora-init.rc"
        }));
        assert!(overrides.iter().any(|e| {
            e["target"] == "/vendor/build.prop"
                && e["source"] == "/usr/lib/droidloom/fedora-vendor-build.prop"
        }));
    }
}
