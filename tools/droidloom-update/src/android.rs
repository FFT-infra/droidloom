use crate::util::*;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
// Full runtime closure. Every update asks the build system for every target.
pub const TARGETS: &[&str] = &[
    "droidloom-native-bridge",
    "android.hardware.graphics.composer3-service.droidloom",
    "droidloom-task-launcher",
    "droidloom-input-bridge",
    "virtual_camera",
    "droidloom-camera-producer",
    "DroidloomIME",
    "DroidloomHome",
    "TouchService",
    "xiaomi-pencilengine-pad",
    "DroidloomSystemUI",
    "droidloom-lmkd-compat",
    "droidloom-classpath-wrapper",
    "netbpfload",
    "init_second_stage",
    "netd",
    "vold",
    "libbinder",
    "servicemanager",
    "libmeminfo",
    "libinputflinger",
    "surfaceflinger",
    "libdroidloom_surface_bridge",
    "libdroidloom_task_control",
    "libdroidloom_task_launcher",
    "libdroidloom_selinux_compat",
    "libdroidloom_composer_aidl",
    "libdroidloom_composer",
    "libdroidloom_minigbm",
    "libdroidloom_syncobj",
    "libdroidloom_denial_ipc",
    "libdroidloom_denial_protocol",
    "libdroidloom_transport",
    "android.hardware.graphics.allocator-service.minigbm",
    "mapper.minigbm",
    "libandroid_net_connectivity_com_android_net_module_util_jni",
    "libservice-connectivity",
    "libnetd_updatable",
    "services",
    "vendorimage",
];
/// Cell-local boot adaptations for the pinned product image.
///
/// Attestation: the image enables RKP-only mode because it expects Google's
/// provisioning service and a secure keymint. Cells run the nonsecure
/// keymint, whose certificate request that service rejects (HTTP 400), and
/// in RKP-only mode keystore treats the failure as fatal instead of falling
/// back to the keymint-provided attestation key. The property is read per
/// key request, and product build.prop outranks every other partition, so
/// the adaptation has to run here, after property loading and before any
/// keystore client.
///
/// Boot state: the pinned kernel command line reports a verified-boot state
/// but no vbmeta device state. The keymint HAL reads both into the RKP boot
/// info, and an unset device state means "unlocked", which contradicts a
/// verified state; the provisioning service then rejects the certificate
/// request as inconsistent. State the matching locked value.
///
/// Sensors: the dynamic sensors sub-HAL publishes its mock accelerometer for
/// every file matching `dummy_accel_file[0-9]` in the directory named by
/// `vendor.dynamic_sensor.mock.file`, and scans that directory when it
/// starts. Create one so SensorManager has an accelerometer; the certificate
/// SDK refuses to start its liveness capture without one.
const CELL_BOOT_ADAPTATIONS: &str = concat!(
    "on early-init\n",
    "    setprop remote_provisioning.tee.rkp_only 0\n",
    "    setprop ro.boot.vbmeta.device_state locked\n",
    "on post-fs-data\n",
    "    mkdir /data/misc/sensors 0775 system system\n",
    "    exec -- /system/bin/toybox touch /data/misc/sensors/dummy_accel_file0\n",
    "    exec -- /system/bin/toybox chown system:system /data/misc/sensors/dummy_accel_file0\n",
    "    exec -- /system/bin/toybox chmod 0664 /data/misc/sensors/dummy_accel_file0\n",
);
// These APEX components are installed by Droidloom's compatibility projection.
// Request their compiled outputs, without asking AOSP to install them into system.
// The intermediate variant directory follows the Android target architecture.
pub fn apex_output(name: &str, target_arch: &str) -> Option<String> {
    // Discovered with `ninja -f combined-<product>.ninja -t targets all`.
    // x86_64 emits bare android_x86_64 variants; ARM64 appends the arch
    // variant (armv8-a from BoardConfig) and APEX-specific suffixes.
    let path = match (name, target_arch) {
        ("netbpfload", "x86_64") => "soong/.intermediates/packages/modules/Connectivity/bpf/loader/netbpfload/android_x86_64/netbpfload".to_string(),
        ("libservice-connectivity", "x86_64") => "soong/.intermediates/packages/modules/Connectivity/service/libservice-connectivity/android_x86_64_shared/libservice-connectivity.so".to_string(),
        ("libnetd_updatable", "x86_64") => "soong/.intermediates/packages/modules/Connectivity/bpf/netd/libnetd_updatable/android_x86_64_shared_cfi/libnetd_updatable.so".to_string(),
        ("netbpfload", "aarch64") => "soong/.intermediates/packages/modules/Connectivity/bpf/loader/netbpfload/android_arm64_armv8-a/netbpfload".to_string(),
        ("libservice-connectivity", "aarch64") => "soong/.intermediates/packages/modules/Connectivity/service/libservice-connectivity/android_arm64_armv8-a_shared/libservice-connectivity.so".to_string(),
        ("libnetd_updatable", "aarch64") => "soong/.intermediates/packages/modules/Connectivity/bpf/netd/libnetd_updatable/android_arm64_armv8-a_shared_cfi/libnetd_updatable.so".to_string(),
        _ => return None,
    };
    Some(path)
}
const PATCHES: &[(&str, &str)] = &[
    (
        "external/v4l2_codec2",
        "android/aosp-patches/0015-v4l2-codec2-zero-unknown-dimensions.patch",
    ),
    (
        "external/v4l2_codec2",
        "android/aosp-patches/0025-v4l2-codec2-query-required-input-buffer-size.patch",
    ),
    (
        "external/v4l2_codec2",
        "android/aosp-patches/0027-v4l2-codec2-av1-decoder.patch",
    ),
    (
        "external/v4l2_codec2",
        "android/aosp-patches/0018-v4l2-codec2-drc-output-reconfigure.patch",
    ),
    (
        "external/v4l2_codec2",
        "android/aosp-patches/0019-v4l2-codec2-gralloc-dmabuf-identity.patch",
    ),
    (
        "external/v4l2_codec2",
        "android/aosp-patches/0020-v4l2-codec2-guard-unmapped-capture-buffer.patch",
    ),
    (
        "external/v4l2_codec2",
        "android/aosp-patches/0021-v4l2-codec2-recycle-initial-eos-buffer.patch",
    ),
    (
        "external/v4l2_codec2",
        "android/aosp-patches/0024-v4l2-codec2-igba-surface-output.patch",
    ),
    (
        "external/v4l2_codec2",
        "android/aosp-patches/0028-v4l2-codec2-keep-input-buffer-size-floor.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0009-droidloom-cpu-placement.patch",
    ),
    (
        "external/minigbm",
        "android/aosp-patches/0010-minigbm-dma-heap-images.patch",
    ),
    (
        "packages/modules/Connectivity",
        "android/aosp-patches/0009-netd-bpf-pid-namespace.patch",
    ),
    (
        "packages/modules/Connectivity",
        "android/aosp-patches/0001-netbpfload-tolerate-shared-kernel-bpf-ids.patch",
    ),
    (
        "packages/modules/Connectivity",
        "android/aosp-patches/0003-connectivity-offline-rcu.patch",
    ),
    (
        "system/netd",
        "android/aosp-patches/0006-netd-cell-kernel-policy.patch",
    ),
    (
        "hardware/interfaces",
        "android/aosp-patches/0002-audio-default-only-service.patch",
    ),
    (
        "hardware/interfaces",
        "android/aosp-patches/0029-audio-bus-socket-driver.patch",
    ),
    (
        "frameworks/av",
        "android/aosp-patches/0031-virtual-camera-cpu-yuv.patch",
    ),
    (
        "system/memory/libmeminfo",
        "android/aosp-patches/0004-libmeminfo-offline-gpu-accounting.patch",
    ),
    (
        "",
        "android/binder-compat/0001-droidloom-context-manager-without-kernel-sid.patch",
    ),
    (
        "",
        "android/binder-compat/0002-droidloom-servicemanager-pid-context-fallback.patch",
    ),
    (
        "",
        "android/binder-compat/0003-droidloom-cell-caller-context-fallback.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0001-droidloom-direct-denial-render-surface.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0002-droidloom-task-input-token.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0003-droidloom-headless-bootstrap.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0004-droidloom-opaque-content.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0005-droidloom-retained-task-composition.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0006-droidloom-late-task-registration.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0007-droidloom-wayland-layers.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0008-droidloom-release-fences.patch",
    ),
    (
        "frameworks/native",
        "android/surfaceflinger/0010-droidloom-task-backpressure-background.patch",
    ),
    (
        "frameworks/native",
        "android/inputflinger/0001-application-token-targeted-injection.patch",
    ),
    (
        "frameworks/native",
        "android/inputflinger/0002-droidloom-targeted-injection-binder.patch",
    ),
    (
        "",
        "android/init-compat/0001-droidloom-cell-namespace.patch",
    ),
    (
        "external/minigbm",
        "android/aosp-patches/0007-minigbm-amdgpu-platform.patch",
    ),
    (
        "system/vold",
        "android/aosp-patches/0008-vold-preserve-shared-storage-submounts.patch",
    ),
    (
        "frameworks/base",
        "android/framework/0001-droidloom-bottom-navigation-insets.patch",
    ),
    (
        "frameworks/base",
        "android/framework/0002-droidloom-routed-touch-configuration.patch",
    ),
    (
        "frameworks/base",
        "android/framework/0003-droidloom-launch-resolution.patch",
    ),
    (
        "hardware/libhardware",
        "android/aosp-patches/0032-dynamic-sensor-accel-onfirstref.patch",
    ),
];
#[derive(Serialize, Deserialize)]
struct Original {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    #[serde(default)]
    modified: Option<std::time::SystemTime>,
}
pub(crate) struct Projection {
    journal: PathBuf,
    originals: Vec<Original>,
}
impl Projection {
    fn new(journal: PathBuf) -> Result<Self> {
        if journal.exists() {
            return fail(format!(
                "interrupted source projection: {}; run recover-source before building",
                journal.display()
            ));
        }
        Ok(Self {
            journal,
            originals: Vec::new(),
        })
    }
    fn save(&mut self, path: &Path) -> Result<()> {
        let path = path.to_owned();
        if self.originals.iter().any(|o| o.path == path) {
            return Ok(());
        }
        self.originals.push(Original {
            modified: fs::metadata(&path).ok().and_then(|m| m.modified().ok()),
            bytes: match fs::read(&path) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            },
            path,
        });
        durable_write(&self.journal, serde_json::to_vec(&self.originals)?)?;
        Ok(())
    }
    pub(crate) fn put(&mut self, path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
        self.save(path)?;
        write(path, bytes)
    }
    fn stabilize_all(&self) -> Result<()> {
        // Wait until all patches are applied: several patches can touch one file.
        for original in &self.originals {
            self.stabilize(&original.path)?;
        }
        Ok(())
    }
    fn stabilize(&self, path: &Path) -> Result<()> {
        if !path.is_file() {
            return Ok(());
        }
        let cache = self
            .journal
            .parent()
            .ok_or("journal lacks parent")?
            .join("projection-cache")
            .join(path.strip_prefix("/").unwrap_or(path));
        // Content comparison is for incremental builds, not artifact verification.
        // Reusing the cached timestamp prevents patch application from invalidating Ninja.
        write(&cache, fs::read(path)?)?;
        fs::File::open(path)?.set_modified(fs::metadata(cache)?.modified()?)?;
        Ok(())
    }
    fn patch(&mut self, source: &Path, cwd: &str, patch: &Path) -> Result<()> {
        let directory = source.join(cwd);
        run(Command::new("git")
            .env("GIT_CEILING_DIRECTORIES", source.parent().unwrap())
            .arg("-C")
            .arg(&directory)
            .args(["apply", "--check"])
            .arg(patch))?;
        let changed = output(
            Command::new("git")
                .env("GIT_CEILING_DIRECTORIES", source.parent().unwrap())
                .arg("-C")
                .arg(&directory)
                .args(["apply", "--numstat"])
                .arg(patch),
        )?;
        for line in changed.lines() {
            let path = line.split('\t').nth(2).ok_or("invalid patch numstat")?;
            if Path::new(path)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return fail("unsafe patch path");
            }
            self.save(&directory.join(path))?;
        }
        run(Command::new("git")
            .env("GIT_CEILING_DIRECTORIES", source.parent().unwrap())
            .arg("-C")
            .arg(&directory)
            .arg("apply")
            .arg(patch))?;
        Ok(())
    }
    fn restore(&mut self) -> Result<()> {
        for o in self.originals.iter().rev() {
            if let Some(bytes) = &o.bytes {
                write(&o.path, bytes)?;
                if let Some(modified) = o.modified {
                    fs::File::open(&o.path)?.set_modified(modified)?;
                }
            } else if o.path.exists() {
                fs::remove_file(&o.path)?;
            }
        }
        self.originals.clear();
        if self.journal.exists() {
            fs::remove_file(&self.journal)?;
        }
        Ok(())
    }
}
impl Drop for Projection {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            eprintln!(
                "SOURCE RESTORE FAILED: {e}; retained {}",
                self.journal.display()
            );
        }
    }
}
pub fn recover(journal: &Path, source: &Path) -> Result<()> {
    let originals: Vec<Original> = serde_json::from_slice(&fs::read(journal)?)?;
    for o in &originals {
        if !o.path.starts_with(source)
            || o.path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return fail("projection journal escapes source tree");
        }
    }
    Projection {
        journal: journal.to_owned(),
        originals,
    }
    .restore()
}
fn metadata_only(text: &str) -> Result<String> {
    let mut result = String::new();
    let mut taking = false;
    for line in text.lines() {
        if line.starts_with("package {") || line.starts_with("license {") {
            taking = true;
        }
        if taking {
            result.push_str(line);
            result.push('\n');
            if line == "}" {
                taking = false;
            }
        }
    }
    if result.is_empty() {
        return fail("missing Blueprint package metadata");
    }
    Ok(result)
}
fn project_blueprints(p: &mut Projection, source: &Path) -> Result<()> {
    for rel in [
        "frameworks/base/libs/WindowManager/Shell/Android.bp",
        "frameworks/base/packages/SystemUI/Android.bp",
        "packages/apps/Launcher3/Android.bp",
        "packages/apps/Settings/Android.bp",
        "frameworks/native/services/serialservice/Android.bp",
        "packages/modules/ConfigInfrastructure/aconfigd/Android.bp",
        "packages/modules/Permission/framework-s/Android.bp",
    ] {
        let path = source.join(rel);
        p.put(&path, metadata_only(&fs::read_to_string(&path)?)?)?;
    }
    let permission = source.join("packages/modules/Permission/framework-s/Android.bp");
    let mut text = fs::read_to_string(&permission)?;
    let mut names = Vec::new();
    for (scope, file) in [
        ("public", "current.txt"),
        ("system", "system-current.txt"),
        ("module-lib", "module-lib-current.txt"),
    ] {
        let name = format!("droidloom-permission-s-{scope}-api");
        names.push(format!("\"{name}\""));
        text += &format!(
            "java_api_contribution {{ name: \"{name}\", api_surface: \"{scope}\", api_file: \"api/{file}\", }}\n"
        );
    }
    text += &format!(
        "java_api_library {{ name: \"droidloom-permission-s-api-stubs\", api_surface: \"module-lib\", api_contributions: [{}], sdk_version: \"module_current\", libs: [\"stub-annotations\"], stubs_type: \"everything\", enable_validation: false, visibility: [\"//prebuilts/sdk:__pkg__\"], }}\n",
        names.join(",")
    );
    p.put(&permission, text)?;
    let sdk = source.join("prebuilts/sdk/Android.bp");
    let mut text = fs::read_to_string(&sdk)?;
    for (scope, suffix, version, combined) in [
        ("public", "", "current", "android_stubs_current"),
        (
            "system",
            ".system",
            "system_current",
            "android_system_stubs_current",
        ),
        (
            "module-lib",
            ".module_lib",
            "module_current",
            "android_module_lib_stubs_current",
        ),
        (
            "system-server",
            ".system_server",
            "system_server_current",
            "android_system_server_stubs_current",
        ),
    ] {
        let dir = source.join("prebuilts/sdk/current").join(scope);
        let mut jars: Vec<_> = fs::read_dir(&dir)?.collect::<std::io::Result<_>>()?;
        jars.sort_by_key(|e| e.file_name());
        for jar in jars {
            let path = jar.path();
            if path.extension().is_none_or(|e| e != "jar") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_str().ok_or("bad SDK name")?;
            let names = if stem == "android" {
                vec![combined.to_string(), format!("{combined}_exportable")]
            } else if ["android-non-updatable", "android.net.ipsec.ike"].contains(&stem)
                || stem.starts_with("framework-")
                || stem.starts_with("service-")
            {
                vec![
                    format!("{stem}.stubs{suffix}"),
                    format!("{stem}.stubs.exportable{suffix}"),
                ]
            } else {
                continue;
            };
            for name in names {
                if stem == "framework-permission-s" && scope == "module-lib" {
                    text += &format!(
                        "\njava_library {{ name: \"{name}\", static_libs: [\"droidloom-permission-s-api-stubs\"], sdk_version: \"{version}\", is_stubs_module: true, installable: false, visibility: [\"//visibility:public\"], }}\n"
                    );
                } else {
                    text += &format!(
                        "\njava_import {{ name: \"{name}\", jars: [\"current/{scope}/{stem}.jar\"], sdk_version: \"{version}\", prefer: true, is_stubs_module: true, installable: false, visibility: [\"//visibility:public\"], }}\n"
                    );
                }
            }
        }
    }
    p.put(&sdk, text)
}
fn bootstrap(source: &Path, out: &Path) -> Result<()> {
    let goroot = source.join("prebuilts/go/linux-x86");
    let generated = out.join("microfactory-main.go");
    let code = fs::read_to_string(source.join("build/blueprint/microfactory/microfactory.go"))?
        .replace("package microfactory", "package main")
        + "\nfunc main() { Main() }\n";
    write(&generated, code)?;
    for (binary, package) in [
        ("soong_ui", "android/soong/cmd/soong_ui"),
        ("mk2rbc", "android/soong/mk2rbc/mk2rbc"),
        ("rbcrun", "rbcrun/rbcrun"),
        (
            "release-config",
            "android/soong/cmd/release_config/release_config",
        ),
    ] {
        let mut command = Command::new(goroot.join("bin/go"));
        command
            .current_dir(source)
            .env("GOROOT", &goroot)
            .env("GOCACHE", out.join("go-cache"))
            .env("GOWORK", "off")
            .env("GO111MODULE", "off")
            .arg("run")
            .arg(&generated)
            .arg("-b")
            .arg(out.join("microfactory_Linux"));
        for (name, path) in [
            ("github.com/google/blueprint", "build/blueprint"),
            ("android/soong", "build/soong"),
            (
                "prebuilts/bazel/common/proto",
                "prebuilts/bazel/common/proto",
            ),
            ("rbcrun", "build/make/tools/rbcrun"),
            ("google.golang.org/protobuf", "external/golang-protobuf"),
            ("go.starlark.net", "external/starlark-go"),
            ("github.com/akrylysov/pogreb", "external/pogreb"),
        ] {
            command
                .arg("-pkg-path")
                .arg(format!("{name}={}", source.join(path).display()));
        }
        command
            .arg("-trimpath")
            .arg(source)
            .arg("-o")
            .arg(out.join(binary))
            .arg(package);
        run_build(&mut command)?;
    }
    Ok(())
}
// Android target architecture selected by the product. The x86_64 cell runs
// ARM64 guests through the source-built translator; ARM64 cells execute
// natively, so their base libraries and vendor modules are aarch64 ELF.
pub(crate) fn target_arch(product: &str) -> Result<&'static str> {
    match product {
        "droidloom_x86_64" => Ok("x86_64"),
        "droidloom_arm64" | "droidloom_sheng" => Ok("aarch64"),
        _ => fail("unknown Android product; expected droidloom_x86_64, droidloom_arm64 or droidloom_sheng"),
    }
}
// The product class follows the display the product presents. The tablet
// product drives one large landscape display; the others present a phone-sized
// display and keep the class their pinned base image already states.
pub(crate) fn declares_tablet(product: &str) -> Result<bool> {
    match product {
        "droidloom_sheng" => Ok(true),
        "droidloom_x86_64" | "droidloom_arm64" => Ok(false),
        _ => fail("unknown Android product; expected droidloom_x86_64, droidloom_arm64 or droidloom_sheng"),
    }
}
pub fn build(
    repo: &Path,
    source: &Path,
    out: &Path,
    work: &Path,
    product: &str,
    jobs: usize,
) -> Result<()> {
    build_targets(repo, source, out, work, product, jobs, TARGETS)
}
pub fn build_targets(
    repo: &Path,
    source: &Path,
    out: &Path,
    work: &Path,
    product: &str,
    jobs: usize,
    targets: &[&str],
) -> Result<()> {
    let lock = repo.join("android/manifest/m2-sparse-source-lock.json");
    let source_lock = droidloom_source::load_lock(&lock)?;
    if !source.exists() {
        droidloom_source::materialize(&lock, &source_lock, source)?;
    } else {
        droidloom_source::reconcile(&lock, &source_lock, source)?;
    }
    droidloom_source::verify_materialized(&lock, &source_lock, source)?;
    let native_bridge_source = crate::native_bridge::prepare_source(repo, source)?;
    for (aidl, adapter) in [
        ("statusbar/IStatusBar.aidl", "EmptyStatusBar"),
        ("policy/IKeyguardService.aidl", "EmptyKeyguard"),
    ] {
        let digest = hash(
            &source
                .join("frameworks/base/core/java/com/android/internal")
                .join(aidl),
        )?;
        let generated = fs::read_to_string(repo.join(format!(
            "android/framework/droidloom-systemui/src/com/android/systemui/compat/{adapter}.java"
        )))?;
        if !generated
            .lines()
            .any(|line| line == format!("// AIDL SHA-256: {digest}"))
        {
            return fail(
                "SystemUI Binder adapters are stale; run tools/droidloom-systemui-stubs for the pinned source",
            );
        }
    }
    let mut p = Projection::new(work.join("source-projection.json"))?;
    for (cwd, patch) in PATCHES {
        p.patch(source, cwd, &repo.join(patch))?;
    }
    project_blueprints(&mut p, source)?;
    for relative in [
        "frameworks/base/tools/aapt2/integration-tests",
        "frameworks/base/core/tests",
        "frameworks/base/media/tests",
        "art/test",
        "art/libartservice/service",
        "packages/modules/Virtualization/guest/trusty/test_vm",
        "packages/modules/Virtualization/guest/trusty/test_vm_os",
        "packages/modules/Connectivity/framework",
        "packages/modules/Connectivity/Tethering/common/TetheringLib",
        "frameworks/base/services/tests",
        "frameworks/base/services/robotests",
    ] {
        p.put(&source.join(relative).join(".find-ignore"), b"")?;
    }
    // Mirror current sources on every invocation; deletion is propagated too.
    let vendor = source.join("vendor/droidloom");
    fs::create_dir_all(&vendor)?;
    for relative in [
        "Android.bp",
        "LICENSE",
        "LICENSES",
        "graphics",
        "runtime/droidloom-cpu-placement",
        "android/framework",
        "android/hardware/camera",
        "android/lmkd-compat",
        "android/runtime",
        "android/native-bridge",
        "android/selinux-compat",
        "android/device",
        "device",
        "android/mesa",
    ] {
        let destination = vendor.join(relative);
        if repo.join(relative).is_dir() {
            fs::create_dir_all(&destination)?;
            run(Command::new("rsync")
                .args(["-a", "--delete"])
                .arg(format!("{}/", repo.join(relative).display()))
                .arg(&destination))?;
        } else {
            copy(&repo.join(relative), &destination)?;
        }
    }
    crate::native_bridge::prepare_build(source, &vendor, work, &mut p, target_arch(product)?)?;
    let mesa = vendor.join("mesa3d");
    fs::create_dir_all(&mesa)?;
    run(Command::new("rsync")
        .args(["-a", "--delete"])
        .arg(format!("{}/", work.join("mesa-source").display()))
        .arg(&mesa))?;
    p.patch(
        source,
        "vendor/droidloom/mesa3d",
        &repo.join("android/aosp-patches/0011-mesa-zink-kgsl.patch"),
    )?;
    p.patch(
        source,
        "vendor/droidloom/mesa3d",
        &repo.join("android/aosp-patches/0012-mesa-adreno722.patch"),
    )?;
    p.patch(
        source,
        "vendor/droidloom/mesa3d",
        &repo.join("android/aosp-patches/0013-mesa-texture-upload-span.patch"),
    )?;
    p.patch(
        source,
        "vendor/droidloom/mesa3d",
        &repo.join("android/aosp-patches/0014-mesa-background-cpu-placement.patch"),
    )?;
    let cross = mesa.join("android/mesa3d_cross.mk");
    let cross_text = fs::read_to_string(&cross)?;
    let python_assignment = "MESA3D_PYTHONPATH := $(AOSP_ABSOLUTE_PATH)/external/python/mako";
    if !cross_text.contains(python_assignment) {
        return fail("Mesa Python build projection no longer matches its pinned source");
    }
    p.put(
        &cross,
        cross_text.replace(
            python_assignment,
            &format!("MESA3D_PYTHONPATH := {}", work.join("python").display()),
        ),
    )?;
    p.put(
        &vendor.join("android/prebuilts/framework-res.apk"),
        fs::read(work.join("framework-res.apk"))?,
    )?;
    let board = vendor.join(format!("android/device/{product}/BoardConfig.mk"));
    let native = work.join("mesa-native.ini");
    write(
        &native,
        format!(
            "[binaries]\nmesa_clc = {}\nvtn_bindgen2 = {}\n",
            meson_string(&work.join("mesa-tools/src/compiler/clc/mesa_clc"))?,
            meson_string(&work.join("mesa-tools/src/compiler/spirv/vtn_bindgen2"))?
        ),
    )?;
    p.put(
        &board,
        fs::read_to_string(&board)?
            + &format!(
                "\nBOARD_MESA3D_MESON_ARGS += -Dmesa-clc=system --native-file {}\n",
                native.display()
            ),
    )?;
    // Generate the init projection from the pinned source with the same checked patch.
    let init = tempfile::Builder::new().prefix("init-").tempdir_in(work)?;
    copy(
        &source.join("system/core/rootdir/init.rc"),
        &init.path().join("rootdir/init.rc"),
    )?;
    run(Command::new("git")
        .env("GIT_CEILING_DIRECTORIES", work)
        .current_dir(init.path())
        .arg("apply")
        .arg(repo.join("android/apex-compat/0001-droidloom-classpath-projection.patch")))?;
    write(
        &work.join("init.rc"),
        format!(
            "import /droidloom/camera/producer.rc\n{}{}",
            CELL_BOOT_ADAPTATIONS,
            fs::read_to_string(init.path().join("rootdir/init.rc"))?
        ),
    )?;
    p.stabilize_all()?;
    bootstrap(source, out)?;
    copy(
        &source.join("prebuilts/build-tools/common/framework/turbine.jar"),
        &out.join("host/linux-x86/framework/turbine.jar"),
    )?;
    let variant_arch = target_arch(product)?;
    let mut cmd = Command::new(out.join("soong_ui"));
    cmd.current_dir(source)
        .env("SOONG_NINJA", "ninja")
        .env("TOP", source)
        .env("OUT_DIR", out)
        .env("ORIGINAL_PWD", source)
        .env(
            "PATH",
            format!(
                "{}:{}:{}",
                work.join("mesa-tools/src/compiler/clc").display(),
                work.join("mesa-tools/src/compiler/spirv").display(),
                std::env::var("PATH")?
            ),
        )
        .env("CCACHE_DIR", work.join("ccache"))
        .env("CCACHE_TEMPDIR", work.join("ccache-tmp"))
        .env("TARGET_PRODUCT", product)
        .env("TARGET_RELEASE", "cp2a")
        .env("TARGET_BUILD_VARIANT", "userdebug")
        .env("JAVA_HOME", source.join("prebuilts/jdk/jdk21/linux-x86"))
        .env("ALLOW_MISSING_DEPENDENCIES", "true")
        .env("SOONG_INCREMENTAL_ANALYSIS", "false")
        .arg("--make-mode")
        .arg(format!("-j{jobs}"))
        .args(targets.iter().map(|name| {
            let path = apex_output(name, variant_arch);
            path.map_or_else(|| PathBuf::from(name), |path| out.join(path))
        }));
    let result = run_build(&mut cmd);
    p.restore()?;
    result?;
    write(
        &out.join("target/product")
            .join(product)
            .join("droidloom-native-bridge-source.json"),
        serde_json::to_vec_pretty(&native_bridge_source)?,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_projection_keeps_ninja_outputs_until_content_changes() {
        let d = tempfile::tempdir().unwrap();
        let input = d.path().join("input");
        write(&input, b"original").unwrap();
        let original_time = fs::metadata(&input).unwrap().modified().unwrap();
        write(
            &d.path().join("build.ninja"),
            b"rule compile\n  command = cp $in $out\nbuild result: compile input\n",
        )
        .unwrap();
        let ninja = || output(Command::new("ninja").current_dir(d.path())).unwrap();
        let journal = d.path().join("journal.json");
        let mut p = Projection::new(journal.clone()).unwrap();
        p.put(&input, b"intermediate patch").unwrap();
        p.put(&input, b"patched").unwrap();
        p.stabilize_all().unwrap();
        ninja();
        let result_time = fs::metadata(d.path().join("result"))
            .unwrap()
            .modified()
            .unwrap();
        p.restore().unwrap();
        assert_eq!(
            fs::metadata(&input).unwrap().modified().unwrap(),
            original_time
        );
        let mut p = Projection::new(journal).unwrap();
        p.put(&input, b"intermediate patch").unwrap();
        p.put(&input, b"patched").unwrap();
        p.stabilize_all().unwrap();
        assert!(ninja().contains("no work to do"));
        assert_eq!(
            fs::metadata(d.path().join("result"))
                .unwrap()
                .modified()
                .unwrap(),
            result_time
        );
        p.put(&input, b"changed patch").unwrap();
        p.stabilize_all().unwrap();
        assert!(!ninja().contains("no work to do"));
        assert_eq!(fs::read(d.path().join("result")).unwrap(), b"changed patch");
    }
    #[test]
    fn restoration_preserves_original_and_removes_created_file() {
        let d = tempfile::tempdir().unwrap();
        let original = d.path().join("original");
        write(&original, b"before").unwrap();
        {
            let mut p = Projection::new(d.path().join("journal")).unwrap();
            p.put(&original, b"after").unwrap();
            p.put(&d.path().join("created"), b"x").unwrap();
        }
        assert_eq!(fs::read(original).unwrap(), b"before");
        assert!(!d.path().join("created").exists());
    }
    #[test]
    fn complete_target_set_contains_both_wire_peers_and_framework() {
        for name in [
            "android.hardware.graphics.composer3-service.droidloom",
            "droidloom-input-bridge",
            "surfaceflinger",
            "libinputflinger",
            "services",
            "vendorimage",
        ] {
            assert!(TARGETS.contains(&name));
        }
    }
    #[test]
    fn every_product_declares_its_architecture_and_device_class() {
        for product in ["droidloom_x86_64", "droidloom_arm64", "droidloom_sheng"] {
            assert!(target_arch(product).is_ok());
            assert!(declares_tablet(product).is_ok());
        }
        assert!(declares_tablet("droidloom_sheng").unwrap());
        for phone in ["droidloom_x86_64", "droidloom_arm64"] {
            assert!(!declares_tablet(phone).unwrap());
        }
        // A product that is not listed must fail rather than silently keep the
        // class of whatever image happens to be pinned for it.
        assert!(declares_tablet("droidloom_new").is_err());
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;

    #[test]
    fn sheng_codec2_patch_set_is_reproducible_and_excludes_diagnostics() {
        let patches = PATCHES
            .iter()
            .filter_map(|(project, patch)| (*project == "external/v4l2_codec2").then_some(*patch))
            .collect::<Vec<_>>();
        assert_eq!(
            patches,
            vec![
                "android/aosp-patches/0015-v4l2-codec2-zero-unknown-dimensions.patch",
                "android/aosp-patches/0025-v4l2-codec2-query-required-input-buffer-size.patch",
                "android/aosp-patches/0027-v4l2-codec2-av1-decoder.patch",
                "android/aosp-patches/0018-v4l2-codec2-drc-output-reconfigure.patch",
                "android/aosp-patches/0019-v4l2-codec2-gralloc-dmabuf-identity.patch",
                "android/aosp-patches/0020-v4l2-codec2-guard-unmapped-capture-buffer.patch",
                "android/aosp-patches/0021-v4l2-codec2-recycle-initial-eos-buffer.patch",
                "android/aosp-patches/0024-v4l2-codec2-igba-surface-output.patch",
                "android/aosp-patches/0028-v4l2-codec2-keep-input-buffer-size-floor.patch",
            ]
        );
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert!(patches.iter().all(|patch| repo_root.join(patch).is_file()));
    }
    #[test]
    fn patch_in_source_without_git_root_is_restored_inside_enclosing_checkout() {
        let d = tempfile::tempdir().unwrap();
        run(Command::new("git").arg("init").arg("--quiet").arg(d.path())).unwrap();
        let source = d.path().join("work/source");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("input"), b"before\n").unwrap();
        let patch = d.path().join("change.patch");
        write(
            &patch,
            b"--- a/input\n+++ b/input\n@@ -1 +1 @@\n-before\n+after\n",
        )
        .unwrap();
        {
            let mut p = Projection::new(d.path().join("journal")).unwrap();
            p.patch(&source, "", &patch).unwrap();
            assert_eq!(fs::read(source.join("input")).unwrap(), b"after\n");
        }
        assert_eq!(fs::read(source.join("input")).unwrap(), b"before\n");
    }
    #[test]
    fn interrupted_projection_recovers_before_next_build() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("source");
        fs::create_dir(&source).unwrap();
        let path = source.join("input");
        write(&path, b"original").unwrap();
        let journal = d.path().join("journal");
        let mut p = Projection::new(journal.clone()).unwrap();
        p.put(&path, b"modified").unwrap();
        std::mem::forget(p);
        recover(&journal, &source).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"original");
        assert!(!journal.exists());
    }
}

fn meson_string(path: &Path) -> Result<String> {
    Ok(format!(
        "'{}'",
        path.to_str()
            .ok_or("non-UTF8 tool path")?
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
    ))
}
