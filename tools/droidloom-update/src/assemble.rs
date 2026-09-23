use crate::util::*;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
pub const HOST_PACKAGES: &[&str] = &[
    "droidloom-supervisor",
    "droidloom-wayland",
    "droidloom-applications",
    "droidloom-doctor",
    "droidloom-update",
    "droidloom-package-support",
];
pub const HOST_BINARIES: &[&str] = &[
    "droidloomd",
    "droidloomctl",
    "droidloom-supervisor",
    "droidloom-wayland",
    "droidloom-applications",
    "droidloom-doctor",
    "droidloom-update",
];
fn json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub(crate) fn download(url: &str, path: &Path, expected: &str) -> Result<()> {
    if path.exists() && hash(path)? == expected {
        return Ok(());
    }
    let scratch = path.with_extension("download");
    run(Command::new("curl")
        .args(["--fail", "--location", "--retry", "3", "--output"])
        .arg(&scratch)
        .arg(url))?;
    if hash(&scratch)? != expected {
        return fail(format!("download checksum mismatch: {}", path.display()));
    }
    fs::rename(scratch, path)?;
    Ok(())
}
fn extract(image: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    run(Command::new("fsck.erofs")
        .arg(format!("--extract={}", destination.display()))
        .arg(image))
}
fn unzip_file(archive: &Path, name: &str, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination.parent().unwrap())?;
    let output = fs::File::create(destination)?;
    run(Command::new("unzip")
        .args(["-p"])
        .arg(archive)
        .arg(name)
        .stdout(Stdio::from(output)))
}
pub fn prepare_inputs(repo: &Path, work: &Path, arch: &str) -> Result<PathBuf> {
    prepare_inputs_from(repo, work, arch, true)
}
pub fn prepare_package_inputs(repo: &Path, work: &Path, arch: &str) -> Result<PathBuf> {
    prepare_inputs_from(repo, work, arch, false)
}
fn prepare_inputs_from(
    repo: &Path,
    work: &Path,
    arch: &str,
    reuse_installed: bool,
) -> Result<PathBuf> {
    // The generic source-lock.json pins the aarch64 CI base; x86_64 has its
    // own lock. `arch` is the Android target architecture, which may differ
    // from the build host during cross builds.
    let lock_name = match arch {
        "x86_64" => "source-lock-x86_64.json",
        "aarch64" => "source-lock.json",
        _ => return fail("unsupported Android target architecture"),
    };
    let lock_path = repo.join(format!("android/manifest/{lock_name}"));
    let lock = json(&lock_path)?;
    let typed = droidloom_image::load_source_lock(&lock_path)?;
    let base = if reuse_installed {
        work.join("base")
    } else {
        work.join(format!("base-{}-{}", arch, typed.aosp_ci_base.build_number))
    };
    // Cached and installed partitions are interchangeable only by their pinned content hash.
    let mut candidates = vec![base.join("images")];
    if reuse_installed {
        candidates.push(PathBuf::from("/var/lib/droidloom/images/current/images"));
    }
    let mut found = None;
    for candidate in candidates {
        let mut valid = true;
        for name in ["system", "system_ext", "product"] {
            let path = candidate.join(format!("{name}.img"));
            let expected = typed
                .aosp_ci_base
                .partitions
                .get(&format!("{name}_a"))
                .ok_or("partition absent from lock")?;
            if !path.is_file() || (reuse_installed && hash(&path)? != *expected) {
                valid = false;
                break;
            }
        }
        if valid {
            found = Some(candidate);
            break;
        }
    }
    let images = if let Some(path) = found {
        path
    } else {
        let archive = repo.join(".work").join(&typed.aosp_ci_base.artifact_name);
        // Public redirect endpoint used by Android's kleaf kernel_prebuilt_repo.bzl.
        let url = format!(
            "https://androidbuildinternal.googleapis.com/android/internal/build/v3/builds/{}/{}/attempts/latest/artifacts/{}/url?redirect=true",
            typed.aosp_ci_base.build_number,
            typed.aosp_ci_base.target,
            typed.aosp_ci_base.artifact_name
        );
        download(&url, &archive, &typed.aosp_ci_base.artifact_sha256)?;
        if base.exists() {
            fs::remove_dir_all(&base)?;
        }
        if reuse_installed {
            droidloom_image::prepare_base(&archive, &base, &typed.aosp_ci_base)?;
        } else {
            extract_package_base(&archive, &base)?;
        }
        base.join("images")
    };
    // Reuse extraction for this selected base; generated files do not need byte-for-byte policing.
    let extracted = work.join("base-system");
    let marker = work.join("base-system.json");
    let system_hash = typed
        .aosp_ci_base
        .partitions
        .get("system_a")
        .ok_or("missing system base identity")?;
    let valid = if marker.exists() && extracted.exists() {
        let m = json(&marker)?;
        m["image"] == *system_hash
            && extracted
                .join(&typed.aosp_ci_base.framework_res.path)
                .is_file()
    } else {
        false
    };
    if !valid {
        if extracted.exists() {
            fs::remove_dir_all(&extracted)?;
        }
        extract(&images.join("system.img"), &extracted)?;
        write(&marker, serde_json::to_vec(&json!({"image":system_hash}))?)?;
    }
    let framework = extracted.join(&typed.aosp_ci_base.framework_res.path);
    if reuse_installed && fs::metadata(&framework)?.len() != typed.aosp_ci_base.framework_res.size {
        return fail("framework-res differs from pinned base");
    }
    copy(&framework, &work.join("framework-res.apk"))?;
    let mesa = &lock["mesa"];
    let version = mesa["version"].as_str().ok_or("Mesa version absent")?;
    let archive = repo.join(format!(".work/mesa-{version}.tar.xz"));
    download(
        mesa["url"].as_str().ok_or("Mesa URL absent")?,
        &archive,
        mesa["sha256"].as_str().ok_or("Mesa digest absent")?,
    )?;
    let mesa_source = work.join("mesa-source");
    if let Err(error) = droidloom_mesa::prepare(
        &lock_path,
        &archive,
        &repo.join("android/mesa/patches"),
        &mesa_source,
    ) {
        if !mesa_source.exists() {
            return Err(error.into());
        }
        eprintln!("Regenerating Mesa source cache: {error}");
        fs::remove_dir_all(&mesa_source)?;
        droidloom_mesa::prepare(
            &lock_path,
            &archive,
            &repo.join("android/mesa/patches"),
            &mesa_source,
        )?;
    }
    // The APEX is taken from the verified system partition, never from a previous installation.
    let apex = extracted.join("system/apex/com.android.virt.apex");
    let compressed = extracted.join("system/apex/com.android.virt.capex");
    let apex = if apex.exists() {
        apex
    } else {
        let a = work.join("virt.apex");
        unzip_file(&compressed, "original_apex", &a)?;
        a
    };
    let payload = work.join("virt-payload.img");
    unzip_file(&apex, "apex_payload.img", &payload)?;
    let virt = work.join("virt");
    if virt.exists() {
        fs::remove_dir_all(&virt)?;
    }
    extract(&payload, &virt)?;
    Ok(images)
}

// The package route consumes the pinned upstream archive directly. Derived
// partitions are not subject to the developer installation's binary matching.
// Publish the cache only after all extraction commands succeed.
fn extract_package_base(archive: &Path, base: &Path) -> Result<()> {
    let temporary = tempfile::Builder::new()
        .prefix("package-base-")
        .tempdir_in(base.parent().ok_or("base cache has no parent")?)?;
    let sparse = temporary.path().join("super.img");
    let raw = temporary.path().join("super.raw.img");
    let images = temporary.path().join("images");
    fs::create_dir(&images)?;
    unzip_file(archive, "super.img", &sparse)?;
    run(Command::new("simg2img").arg(&sparse).arg(&raw))?;
    run(Command::new("lpunpack")
        .args(["-p", "system_a", "-p", "system_ext_a", "-p", "product_a"])
        .arg(&raw)
        .arg(&images))?;
    for name in ["system", "system_ext", "product"] {
        fs::rename(
            images.join(format!("{name}_a.img")),
            images.join(format!("{name}.img")),
        )?;
    }
    fs::remove_file(sparse)?;
    fs::remove_file(raw)?;
    fs::rename(temporary.path(), base)?;
    Ok(())
}
pub fn mesa_tools(work: &Path, jobs: usize) -> Result<()> {
    run(Command::new("pkg-config").args(["--exists", "LLVMSPIRVLib"]))?;
    let build = work.join("mesa-tools");
    let mut configure = Command::new("meson");
    // These compiler helpers always run on the build host (their outputs feed
    // the Android build). A cross sysroot leaked through the environment would
    // silently link foreign libraries into native tools, so scrub it here.
    // The Rust cross flow keeps its own target-scoped variables untouched.
    for var in [
        "PKG_CONFIG_SYSROOT_DIR",
        "PKG_CONFIG_PATH",
        "PKG_CONFIG_ALLOW_CROSS",
        "TARGET_PKG_CONFIG_SYSROOT_DIR",
        "TARGET_PKG_CONFIG_PATH",
    ] {
        configure.env_remove(var);
    }
    configure
        .env("CCACHE_DIR", work.join("ccache"))
        .env("CCACHE_TEMPDIR", work.join("ccache-tmp"));
    configure
        .env("PYTHONPATH", work.join("python"))
        .env("PYTHONDONTWRITEBYTECODE", "1");
    configure.arg("setup");
    if build.join("build.ninja").exists() {
        configure.arg("--reconfigure");
    }
    configure.arg(&build).arg(work.join("mesa-source")).args([
        "-Dplatforms=",
        "-Dgallium-drivers=",
        "-Dvulkan-drivers=",
        "-Dllvm=enabled",
        "-Dshared-llvm=enabled",
        "-Dmesa-clc=enabled",
        "-Dinstall-mesa-clc=true",
        "-Dprecomp-compiler=enabled",
        "-Dopengl=false",
        "-Degl=disabled",
        "-Dgbm=disabled",
        "-Dglx=disabled",
        "-Dgles1=disabled",
        "-Dgles2=disabled",
        "-Dvideo-codecs=",
        "-Dbuild-tests=false",
        "-Dtools=",
        "-Dvalgrind=disabled",
        "-Dlibunwind=disabled",
    ]);
    run_build(&mut configure)?;
    let mut ninja = Command::new("ninja");
    for var in [
        "PKG_CONFIG_SYSROOT_DIR",
        "PKG_CONFIG_PATH",
        "PKG_CONFIG_ALLOW_CROSS",
        "TARGET_PKG_CONFIG_SYSROOT_DIR",
        "TARGET_PKG_CONFIG_PATH",
    ] {
        ninja.env_remove(var);
    }
    run_build(
        ninja
            .arg("-C")
            .arg(&build)
            .arg(format!("-j{jobs}"))
            .args([
                "src/compiler/clc/mesa_clc",
                "src/compiler/spirv/vtn_bindgen2",
            ])
            .env("PYTHONPATH", work.join("python"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("CCACHE_DIR", work.join("ccache"))
            .env("CCACHE_TEMPDIR", work.join("ccache-tmp")),
    )
}
pub fn assemble(
    repo: &Path,
    work: &Path,
    base: &Path,
    product: &Path,
    host: &Path,
    payload: &Path,
    uid: u32,
    target_arch: &str,
) -> Result<()> {
    assemble_artifacts(repo, work, base, product, host, payload, target_arch)?;
    let notices = payload.join("usr/share/licenses/droidloom");
    crate::licenses::project(repo, &notices)?;
    crate::licenses::rust(repo, &notices.join("rust"))?;
    configure_local(repo, payload, uid, target_arch)
}

/// Stage the complete artifact recipe without reading any build-host configuration.
pub fn assemble_artifacts(
    repo: &Path,
    work: &Path,
    base: &Path,
    product: &Path,
    host: &Path,
    payload: &Path,
    target_arch: &str,
) -> Result<()> {
    let product_name = product
        .file_name()
        .ok_or("Android product output has no name")?
        .to_string_lossy()
        .into_owned();
    let runtime = payload.join("usr/lib/droidloom/runtime");
    for name in HOST_BINARIES {
        let dest = payload.join("usr/bin").join(name);
        copy(&host.join(name), &dest)?;
        mode(&dest, 0o755)?;
    }
    for (src, dst) in [
        ("system/bin/init", "bin/droidloom-android-init"),
        (
            "vendor/bin/droidloom-lmkd-compat",
            "bin/droidloom-lmkd-compat",
        ),
        ("system/bin/netd", "bin/netd-cell-policy"),
        ("system/bin/servicemanager", "bin/servicemanager"),
        ("system/bin/surfaceflinger", "bin/surfaceflinger"),
        (
            "vendor/bin/hw/android.hardware.graphics.composer3-service.droidloom",
            "bin/android.hardware.graphics.composer3-service.droidloom",
        ),
        (
            "vendor/bin/droidloom-task-launcher",
            "bin/droidloom-task-launcher",
        ),
        ("system/framework/services.jar", "framework/services.jar"),
        (
            "vendor/bin/droidloom-input-bridge",
            "ime/droidloom-input-bridge",
        ),
        (
            "vendor/framework/droidloom-input-bridge.jar",
            "framework/droidloom-input-bridge.jar",
        ),
        (
            "vendor/framework/droidloom-input-bridge.jar",
            "ime/droidloom-input-bridge.jar",
        ),
        (
            "system/app/DroidloomIME/DroidloomIME.apk",
            "ime/DroidloomIME.apk",
        ),
        (
            "system/app/DroidloomHome/DroidloomHome.apk",
            "ime/DroidloomHome.apk",
        ),
        (
            "system/system_ext/priv-app/DroidloomSystemUI/DroidloomSystemUI.apk",
            "systemui/SystemUI.apk",
        ),
        ("vendor/build.prop", "overrides/vendor/build.prop"),
        (
            "vendor/bin/droidloom-classpath-wrapper",
            "compat/classpath-compat/bin/droidloom-classpath-wrapper",
        ),
        (
            "system/lib64/libandroid_net_connectivity_com_android_net_module_util_jni.so",
            "compat/classpath-compat/lib64/libandroid_net_connectivity_com_android_net_module_util_jni.so",
        ),
    ] {
        copy(&product.join(src), &runtime.join(dst))?;
    }
    for (name, destination) in [
        ("netbpfload", "compat/classpath-compat/bin/netbpfload"),
        (
            "libnetd_updatable",
            "compat/classpath-compat/lib64/libnetd_updatable.so",
        ),
        (
            "libservice-connectivity",
            "compat/classpath-compat/lib64/libservice-connectivity.so",
        ),
    ] {
        copy(
            &work
                .join("android-out")
                .join(crate::android::apex_output(name, target_arch).unwrap()),
            &runtime.join(destination),
        )?;
    }
    copy(
        &product.join("system/bin/vold"),
        &payload.join("usr/lib/droidloom/storage/vold"),
    )?;
    for name in ["libbinder.so", "libmeminfo.so", "libinputflinger.so"] {
        copy(
            &product.join("system/lib64").join(name),
            &runtime.join("lib64").join(name),
        )?;
    }
    // The userspace GPU driver differs per target: AMD on x86_64 desktops,
    // the generic libdrm core on ARM64 MSM hosts (the Freedreno winsys code
    // ships inside libgallium_dri; no separate libdrm_freedreno is emitted).
    // A missing entry fails loudly at copy time.
    let mut vendor_libs = vec![
        "libdroidloom_surface_bridge.dylib.so",
        "libdroidloom_task_control.dylib.so",
        "libdroidloom_composer_aidl.dylib.so",
        "libdroidloom_syncobj.dylib.so",
        "libdroidloom_denial_ipc.dylib.so",
        "libdroidloom_composer.dylib.so",
        "libdroidloom_minigbm.dylib.so",
        "libdroidloom_denial_protocol.dylib.so",
        "libdroidloom_transport.dylib.so",
        "libdroidloom_task_launcher.dylib.so",
        "libdroidloom_selinux_compat.so",
        "libminigbm_gralloc.so",
        "libgallium_dri.so",
    ];
    vendor_libs.push(match target_arch {
        "x86_64" => "libdrm_amdgpu.so",
        "aarch64" => "libdrm.so",
        _ => return fail("unsupported Android target architecture"),
    });
    for name in vendor_libs {
        copy(
            &product.join("vendor/lib64").join(name),
            &runtime.join("lib64").join(name),
        )?;
    }
    for name in [
        "libEGL_mesa.so",
        "libGLESv1_CM_mesa.so",
        "libGLESv2_mesa.so",
    ] {
        copy(
            &product.join("vendor/lib64/egl").join(name),
            &runtime.join("egl").join(name),
        )?;
    }
    for (src, dst) in [
        ("packaging/compat/lmkd.rc", "compat/lmkd-first-pixels.rc"),
        ("packaging/compat/netd.rc", "compat/netd-mainline-bpf.rc"),
        (
            "android/vintf-compat/compatibility_matrix.device.xml",
            "compat/vintf/compatibility_matrix.device.xml",
        ),
        (
            &format!("android/device/{product_name}/droidloom-composer.rc"),
            "etc/init/droidloom-composer.rc",
        ),
        ("packaging/ime/setup", "ime/setup"),
        ("packaging/home/setup", "ime/home-setup"),
        (
            "android/framework/droidloom-home/droidloom-home.rc",
            "ime/droidloom-home.rc",
        ),
        (
            "android/framework/droidloom-input-bridge/droidloom-input-bridge.rc",
            "ime/droidloom-input-bridge.rc",
        ),
    ] {
        copy(&repo.join(src), &runtime.join(dst))?;
    }
    // The Composer init fragment lives beside its product definition.
    copy(
        &repo.join(format!(
            "android/device/{product_name}/droidloom-composer.rc"
        )),
        &runtime.join("etc/init/droidloom-composer.rc"),
    )?;
    copy(
        &work.join("init.rc"),
        &runtime.join("compat/init-classpath-first-pixels.rc"),
    )?;
    // Preserve both Java classpaths advertised by the base APEX manifest.
    for name in [
        "etc/classpaths/bootclasspath.pb",
        "etc/classpaths/systemserverclasspath.pb",
        "javalib/framework-virtualization.jar",
        "javalib/service-virtualization.jar",
    ] {
        copy(
            &work.join("virt").join(name),
            &runtime
                .join("compat/classpath-compat/apex/com.android.virt")
                .join(name),
        )?;
    }
    let images = payload.join("var/lib/droidloom/images/images");
    for name in ["product.img"] {
        copy(&base.join(name), &images.join(name))?;
    }
    crate::native_bridge::stage_image(
        repo,
        &base.join("system.img"),
        &images.join("system.img"),
        product,
    )?;
    crate::image_policy::stage(
        &base.join("system_ext.img"),
        &images.join("system_ext.img"),
        &product.join("vendor/build.prop"),
    )?;
    let vendor = product.join("vendor.img");
    let mut magic = [0; 4];
    use std::io::Read;
    fs::File::open(&vendor)?.read_exact(&mut magic)?;
    if magic == [0x3a, 0xff, 0x26, 0xed] {
        run(Command::new("simg2img")
            .arg(&vendor)
            .arg(images.join("vendor.raw.img")))?;
    } else {
        copy(&vendor, &images.join("vendor.raw.img"))?;
    }
    for name in [
        "system/droidloomd.service",
        "user/droidloom.service",
        "user/droidloom-applications.service",
    ] {
        copy(
            &repo.join("packaging/systemd").join(name),
            &payload.join("usr/lib/systemd").join(name),
        )?;
    }
    Ok(())
}

fn configure_local(repo: &Path, payload: &Path, uid: u32, target_arch: &str) -> Result<()> {
    let runtime = payload.join("usr/lib/droidloom/runtime");
    // The ARM64 cell recipe is the Moto-validated variant; only identity
    // fields are rewritten below, never the runtime mappings.
    let spec_name = match target_arch {
        "x86_64" => "packaging/cell-spec-x86_64-u1000.json",
        "aarch64" => "packaging/cell-spec-arm64-u1001.json",
        _ => return fail("unsupported Android target architecture"),
    };
    let mut spec = json(&repo.join(spec_name))?;
    let render = render_node(target_arch)?;
    // Normalize every identity field to the target desktop user. The base
    // recipe pins the Moto developer ids; cross bundles must not inherit them.
    let base_uid = spec["host_uid"].as_u64().unwrap_or(1000);
    let reroute = |value: &str| {
        value
            .replace(&format!("/users/{base_uid}/"), &format!("/users/{uid}/"))
            .replace(&format!("/user/{base_uid}/"), &format!("/user/{uid}/"))
            .replace(&format!("cells/u{base_uid}"), &format!("cells/u{uid}"))
    };
    spec["host_uid"] = json!(uid);
    // The build host may not know the target desktop user (cross bundles).
    // Explicit overrides win; otherwise resolve through the host user database.
    let passwd = match std::env::var("DROIDLOOM_TARGET_USER") {
        Ok(user) if !user.is_empty() => format!("{user}:x:{uid}:"),
        _ => output(Command::new("getent").args(["passwd", &uid.to_string()]))?,
    };
    let mut fields = passwd.split(':');
    let user = serde_json::to_string(fields.next().ok_or("user absent")?)?;
    let gid: u32 = match std::env::var("DROIDLOOM_TARGET_GID") {
        Ok(gid) if !gid.is_empty() => gid.parse().map_err(|_| "bad DROIDLOOM_TARGET_GID")?,
        _ => fields
            .nth(2)
            .ok_or("group absent")?
            .parse()
            .map_err(|_| "bad group id")?,
    };
    // NOTE: no top-level host_gid is written: the lifecycle protocol message
    // has no such field (per-directory host_gid entries are untouched).
    for field in ["data_dir", "runtime_dir", "denial_socket"] {
        if let Some(path) = spec[field].as_str() {
            spec[field] = json!(reroute(path));
        }
    }
    // The shared-storage probe path carries the base recipe's uid as well.
    if let Some(probe) = spec["shared_storage_probe"].as_str() {
        spec["shared_storage_probe"] = json!(reroute(probe));
    }
    spec["render_node"] = json!(render);
    spec["shared_storage_directories"] = json!([]);
    if Path::new("/etc/droidloom/cell.json").exists() {
        let existing = json(Path::new("/etc/droidloom/cell.json"))?;
        preserve_settings(&mut spec, &existing, uid)?;
    }
    for entry in spec["android_file_overrides"]
        .as_array()
        .ok_or("missing overrides")?
    {
        let src = entry["source"].as_str().ok_or("bad override")?;
        let actual = if let Some(rel) = src.strip_prefix("/usr/lib/droidloom/current/") {
            runtime.join(rel)
        } else {
            payload.join(src.trim_start_matches('/'))
        };
        if !actual.is_file() {
            return fail(format!("missing runtime override: {src}"));
        }
    }
    write(
        &payload.join("etc/droidloom/cell.json"),
        serde_json::to_vec_pretty(&spec)?,
    )?;
    write(
        &payload.join("etc/droidloom/update.json"),
        serde_json::to_vec_pretty(&json!({"source":repo,"uid":uid}))?,
    )?;
    write(
        &payload.join("usr/lib/environment.d/60-droidloom.conf"),
        format!(
            "DROIDLOOM_HOST_PEER_PID=0\nDROIDLOOM_HOST_PEER_UID={uid}\nDROIDLOOM_HOST_PEER_GID={gid}\nDROIDLOOM_RENDER_NODE={render}\n"
        ),
    )?;
    write(
        &payload.join("etc/polkit-1/rules.d/49-droidloom.rules"),
        format!(
            "polkit.addRule(function(action, subject) {{\n if (action.id === \"org.freedesktop.systemd1.manage-units\" && subject.user === {user} && action.lookup(\"unit\") === \"droidloomd.service\" && (action.lookup(\"verb\") === \"start\" || action.lookup(\"verb\") === \"stop\")) return polkit.Result.YES;\n}});\n"
        ),
    )?;
    Ok(())
}
fn render_node(target_arch: &str) -> Result<String> {
    // Cross bundles target a different machine: the target operator pins its
    // render node explicitly instead of inheriting the build host's.
    if let Ok(node) = std::env::var("DROIDLOOM_RENDER_NODE")
        && !node.is_empty()
    {
        return Ok(node);
    }
    if target_arch != std::env::consts::ARCH {
        return fail("cross bundle requires DROIDLOOM_RENDER_NODE for the target device");
    }
    let mut nodes = fs::read_dir("/sys/class/drm")?.collect::<std::io::Result<Vec<_>>>()?;
    nodes.sort_by_key(|e| e.file_name());
    for node in nodes {
        if !node.file_name().to_string_lossy().starts_with("renderD") {
            continue;
        }
        if let Ok(driver) = fs::canonicalize(node.path().join("device/driver"))
            && driver.file_name().is_some_and(|n| {
                n == "amdgpu" || n == "i915" || n == "xe" || n == "msm" || n == "msm_dpu" || n == "freedreno"
            })
        {
            return Ok(format!("/dev/dri/{}", node.file_name().to_string_lossy()));
        }
    }
    fail("no supported render node found (AMD, Intel or MSM); pin one with DROIDLOOM_RENDER_NODE")
}
fn preserve_settings(spec: &mut Value, existing: &Value, uid: u32) -> Result<()> {
    if existing["host_uid"].as_u64() != Some(u64::from(uid)) {
        return fail("the existing Droidloom installation belongs to another desktop user");
    }
    // Runtime mappings come from the new recipe. Personal data and sharing do not.
    for field in [
        "data_dir",
        "subordinate_uids",
        "subordinate_gids",
        "shared_storage_directories",
    ] {
        if let Some(value) = existing.get(field) {
            spec[field] = value.clone();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn update_keeps_personal_state_but_replaces_runtime_mappings() {
        let existing = json!({"host_uid":1000,"data_dir":"/var/lib/droidloom/custom-data","shared_storage_directories":[{"source":"/home/test/Downloads"}],"android_file_overrides":["old"]});
        let mut candidate = json!({"android_file_overrides":["new"]});
        preserve_settings(&mut candidate, &existing, 1000).unwrap();
        assert_eq!(candidate["data_dir"], existing["data_dir"]);
        assert_eq!(
            candidate["shared_storage_directories"],
            existing["shared_storage_directories"]
        );
        assert_eq!(candidate["android_file_overrides"], json!(["new"]));
        assert!(preserve_settings(&mut candidate, &existing, 1001).is_err());
    }
}
