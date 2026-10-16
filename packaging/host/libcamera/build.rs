// Run only inside the pinned, rootless ARM64 build container. No host installation.
use std::{env, error::Error, ffi::OsStr, fs, path::Path, process::Command};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const PATCH: &str = "0002-simple-cancel-tracked-requests-on-stop.patch";
const WORKER_PATCH: &str = "0003-simple-quiesce-capture-before-isp-stop.patch";
const SRPM_SHA: &str = "2ab6c01e0191dec0599076a7451d0c3978ef4e045802d3424939d748bd756647";

fn run(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("command failed: {command:?} ({status})").into());
    }
    Ok(())
}

fn collect_rpms(directory: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_rpms(&path, destination)?;
        } else if path.extension() == Some(OsStr::new("rpm")) {
            fs::copy(&path, destination.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() != 1 {
        return Err("usage: libcamera-rpm-build NEW_WORK_DIRECTORY".into());
    }
    if env::consts::ARCH != "aarch64" {
        return Err("this entrypoint requires the pinned ARM64 build container".into());
    }
    let root = Path::new(&args[0]);
    fs::create_dir(root)?;
    let root = fs::canonicalize(root)?;
    let top = root.join("rpmbuild");
    fs::create_dir(&top)?;
    run(Command::new("cp").args(["-a", "/opt/source/."]).arg(&top))?;
    let original = Path::new("/input/libcamera-0.7.1-1.fc44.src.rpm");
    let hash = Command::new("sha256sum").arg(original).output()?;
    if !hash.status.success() || !String::from_utf8_lossy(&hash.stdout).starts_with(SRPM_SHA) {
        return Err("source RPM checksum mismatch".into());
    }
    fs::write(root.join("source-rpm.sha256"), &hash.stdout)?;
    let packages = Command::new("rpm")
        .args(["-qa", "--qf", "%{NEVRA}\n"])
        .output()?;
    if !packages.status.success() {
        return Err("cannot record build environment".into());
    }
    fs::write(root.join("build-environment.txt"), packages.stdout)?;

    // The original source must reproduce the specific missing-metadata failures.
    let pristine = root.join("pristine");
    fs::create_dir(&pristine)?;
    run(Command::new("tar")
        .arg("-xjf")
        .arg(top.join("SOURCES/libcamera-v0.7.1.tar.bz2"))
        .arg("-C")
        .arg(&pristine))?;
    let red = Command::new("/usr/local/bin/libcamera-stop-regression")
        .arg(pristine.join("libcamera-v0.7.1/src/libcamera/pipeline/simple/simple.cpp"))
        .arg(root.join("red"))
        .output()?;
    fs::write(
        root.join("red.log"),
        [&red.stdout[..], &red.stderr[..]].concat(),
    )?;
    let red_stdout = String::from_utf8_lossy(&red.stdout);
    if red.status.code() != Some(1)
        || !red_stdout.contains("FAIL ISP-cancelled output does not wait for missing metadata")
        || !red_stdout.contains("FAIL metadata-only outstanding request cancelled exactly once")
        || !red_stdout.contains("FAIL framework pending queue empty after stop")
    {
        return Err(
            "unpatched source did not reproduce the expected regression; inspect red.log".into(),
        );
    }
    println!("Red regression reproduced; applying the stop-only patches.");
    for patch in [PATCH, WORKER_PATCH] {
        fs::copy(
            Path::new("/input").join(patch),
            top.join("SOURCES").join(patch),
        )?;
    }
    for file in [
        "stop-regression.rs",
        "stop-regression.cc.in",
        "worker-regression.rs",
        "worker-regression.cc.in",
    ] {
        fs::copy(
            Path::new("/input").join(file),
            top.join("SOURCES").join(file),
        )?;
    }
    let spec_path = top.join("SPECS/libcamera.spec");
    let original_spec = fs::read_to_string(&spec_path)?;
    if !original_spec.contains("Version: 0.7.1\n")
        || !original_spec.contains("Release: 1%{?dist}\n")
        || original_spec
            .matches("Source3: 70-libcamera.rules\n")
            .count()
            != 1
        || original_spec
            .matches("Patch01: 0001-disable-rpi-pisp.patch\n")
            .count()
            != 1
        || original_spec
            .matches("BuildRequires: libevent-devel\n")
            .count()
            != 1
        || original_spec.matches("\n%files\n").count() != 1
    {
        return Err("unexpected Fedora source spec".into());
    }
    let spec = original_spec.replace(
        "Patch01: 0001-disable-rpi-pisp.patch\n",
        &format!(
            "Patch01: 0001-disable-rpi-pisp.patch\nPatch02: {PATCH}\nPatch03: {WORKER_PATCH}\n"
        ),
    );
    let spec = spec.replace("Source3: 70-libcamera.rules\n",
        "Source3: 70-libcamera.rules\nSource4: stop-regression.rs\nSource5: stop-regression.cc.in\nSource6: worker-regression.rs\nSource7: worker-regression.cc.in\n");
    let spec = spec.replace(
        "BuildRequires: libevent-devel\n",
        "BuildRequires: libevent-devel\nBuildRequires: elfutils-devel\nBuildRequires: rust\n",
    );
    let spec = spec.replace(
        "\n%files\n",
        concat!(
            "\n%check\n",
            "rustc --edition=2024 %{SOURCE4} -o %{_builddir}/libcamera-stop-regression\n",
            "%{_builddir}/libcamera-stop-regression ",
            "%{_builddir}/%{name}-v%{version}/src/libcamera/pipeline/simple/simple.cpp ",
            "%{_builddir}/stop-regression\n",
            "mkdir %{_builddir}/restart-baseline\n",
            "tar -xjf %{SOURCE0} -C %{_builddir}/restart-baseline\n",
            "patch --batch --forward --fuzz=0 -p1 -d %{_builddir}/restart-baseline/%{name}-v%{version} ",
            "< %{_sourcedir}/0002-simple-cancel-tracked-requests-on-stop.patch\n",
            "rustc --edition=2024 %{SOURCE6} -o %{_builddir}/libcamera-worker-regression\n",
            "%{_builddir}/libcamera-worker-regression ",
            "%{_builddir}/restart-baseline/%{name}-v%{version} ",
            "%{_builddir}/%{name}-v%{version} ",
            "%{_builddir}/%{name}-v%{version}/%{_vpath_builddir} ",
            "%{_builddir}/worker-regression\n",
            "cp %{_builddir}/worker-regression/baseline.log %{_topdir}/worker-baseline.log\n",
            "cp %{_builddir}/worker-regression/candidate.log %{_topdir}/worker-candidate.log\n",
            "\n%files\n"
        ),
    );
    fs::write(&spec_path, spec)?;
    let log = fs::File::create(root.join("build.log"))?;
    println!(
        "Building unchanged NEVRA with two jobs; log: {}",
        root.join("build.log").display()
    );
    run(Command::new("rpmbuild")
        .arg("-ba")
        .arg(&spec_path)
        .args([
            "--define",
            &format!("_topdir {}", top.display()),
            "--define",
            "_smp_mflags -j2",
            "--define",
            "_smp_build_ncpus 2",
        ])
        .env("RPM_BUILD_NCPUS", "2")
        .env("CMAKE_BUILD_PARALLEL_LEVEL", "2")
        .stdout(log.try_clone()?)
        .stderr(log))?;

    let candidate = root.join("candidate");
    fs::create_dir(&candidate)?;
    collect_rpms(&top.join("RPMS"), &candidate)?;
    collect_rpms(&top.join("SRPMS"), &candidate)?;
    for patch in [PATCH, WORKER_PATCH] {
        fs::copy(Path::new("/input").join(patch), candidate.join(patch))?;
    }
    let mut files = fs::read_dir(&candidate)?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    files.sort();
    if !files
        .iter()
        .any(|f| f == "libcamera-0.7.1-1.fc44.aarch64.rpm")
        || !files
            .iter()
            .any(|f| f == "libcamera-ipa-0.7.1-1.fc44.aarch64.rpm")
    {
        return Err("required ARM64 runtime RPMs missing".into());
    }
    let checksums = Command::new("sha256sum")
        .args(&files)
        .current_dir(&candidate)
        .output()?;
    if !checksums.status.success() {
        return Err("candidate checksum generation failed".into());
    }
    fs::write(candidate.join("SHA256SUMS"), &checksums.stdout)?;
    print!("{}", String::from_utf8_lossy(&checksums.stdout));
    println!(
        "Candidate produced without installing it: {}",
        candidate.display()
    );
    Ok(())
}
