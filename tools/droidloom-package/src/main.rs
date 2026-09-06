//! Rust orchestration around makepkg's standard package interface.
mod validation;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs,
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Instant,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(
    version,
    about = "Build Droidloom pacman packages without installing them"
)]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Compile the complete runtime and produce packages in a rootless Arch builder.
    Build {
        #[arg(long)]
        source: Option<PathBuf>,
        #[arg(long)]
        jobs: Option<usize>,
        /// Remove this workflow's compiler outputs while retaining downloaded inputs.
        #[arg(long)]
        clean: bool,
        /// Seed the source cache from an existing sparse AOSP checkout (never binaries).
        #[arg(long)]
        source_cache: Option<PathBuf>,
    },
    /// Exercise package install, setup, reinstall and removal in disposable Arch.
    Check {
        /// Directory containing the matching runtime and image packages.
        #[arg(long)]
        packages: PathBuf,
        /// Optional older package pair for real upgrade/downgrade transactions.
        #[arg(long)]
        previous: Option<PathBuf>,
    },
    #[command(hide = true)]
    InContainer {
        #[arg(long)]
        jobs: usize,
        #[arg(long)]
        clean: bool,
    },
    #[command(hide = true)]
    Compile {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        work: PathBuf,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        jobs: usize,
    },
    #[command(hide = true)]
    InstallTree {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        destination: PathBuf,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Version {
    version: String,
    release: u32,
    architecture: String,
}
impl Version {
    fn read(repo: &Path) -> Result<Self> {
        let version: Self =
            serde_json::from_slice(&fs::read(repo.join("packaging/arch/version.json"))?)?;
        if version.version.is_empty()
            || !version
                .version
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._+".contains(&c))
            || version.release == 0
            || version.architecture != "x86_64"
        {
            return Err("invalid package version or architecture".into());
        }
        Ok(version)
    }
    fn directory(&self) -> String {
        format!("{}-{}", self.version, self.release)
    }
}

fn run(command: &mut Command) -> Result<()> {
    eprintln!("+ {command:?}");
    let status = command.stdin(Stdio::null()).status()?;
    if !status.success() {
        return Err(format!("command failed ({status}): {command:?}").into());
    }
    Ok(())
}

fn run_logged(command: &mut Command, log: &fs::File) -> Result<()> {
    eprintln!("+ {command:?}");
    let mut record = log.try_clone()?;
    writeln!(record, "+ {command:?}")?;
    let record = Arc::new(Mutex::new(record));
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let streams: Vec<Box<dyn Read + Send>> = vec![
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stderr.take().unwrap()),
    ];
    let readers: Vec<_> = streams
        .into_iter()
        .map(|mut stream| {
            let record = Arc::clone(&record);
            std::thread::spawn(move || -> std::io::Result<()> {
                let mut buffer = [0_u8; 8192];
                loop {
                    let size = stream.read(&mut buffer)?;
                    if size == 0 {
                        return Ok(());
                    }
                    record.lock().unwrap().write_all(&buffer[..size])?;
                    // A closed terminal must not discard the saved build log.
                    let _ = std::io::stderr().write_all(&buffer[..size]);
                }
            })
        })
        .collect();
    let status = child.wait()?;
    for reader in readers {
        reader.join().map_err(|_| "build log reader failed")??;
    }
    if !status.success() {
        return Err(format!("command failed ({status}): {command:?}").into());
    }
    Ok(())
}
fn require_user() -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        return Err("Build as your normal user, without sudo. Only installing the finished packages requires administrator access.".into());
    }
    Ok(())
}
fn repository(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let directory = explicit
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()?;
    directory
        .ancestors()
        .find(|p| p.join("packaging/arch/PKGBUILD.in").is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| "run inside the Droidloom checkout or use --source".into())
}
fn compiler_cpus() -> Result<Vec<usize>> {
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&mask), &mut mask) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut cpus: Vec<_> = (0..libc::CPU_SETSIZE as usize)
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &mask) })
        .collect();
    if cpus.len() < 3 {
        return Err(
            "At least three available logical CPUs are needed to keep two free while compiling."
                .into(),
        );
    }
    cpus.truncate(cpus.len() - 2);
    Ok(cpus)
}
fn podman(work: &Path) -> Command {
    let mut command = Command::new("podman");
    command
        .arg("--root")
        .arg(work.join("containers"))
        .arg("--runroot")
        .arg(work.join("container-run"));
    command
}
fn copy_inputs(repo: &Path, snapshot: &Path) -> Result<()> {
    fs::create_dir_all(snapshot)?;
    let mut command = Command::new("rsync");
    command.args(["-a", "--delete"]);
    for exclusion in [
        ".work/",
        "/work/",
        ".git/",
        "target/",
        "dist/",
        "apks/",
        "downloads/",
        ".codex/",
        ".agents/",
    ] {
        command.arg("--exclude").arg(exclusion);
    }
    run(command.arg(format!("{}/", repo.display())).arg(snapshot))
}
fn build(
    explicit: Option<PathBuf>,
    requested: Option<usize>,
    clean: bool,
    source_cache: Option<PathBuf>,
) -> Result<()> {
    let started = Instant::now();
    require_user()?;
    if std::env::consts::ARCH != "x86_64" {
        return Err("This package build currently requires an x86_64 Linux host.".into());
    }
    let repo = repository(explicit)?;
    let version = Version::read(&repo)?;
    let cpus = compiler_cpus()?;
    let jobs = requested.unwrap_or(cpus.len()).min(cpus.len());
    if jobs == 0 {
        return Err("--jobs must be positive".into());
    }
    if !Command::new("podman")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return Err("Podman is missing. Install it with sudo pacman -S --needed podman. Sudo is needed to write Podman's system files, its container runtime/networking dependencies, and pacman's database. Droidloom compilation and packaging need no sudo.".into());
    }
    if !Command::new("rsync")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return Err("rsync is missing. It copies the source snapshot into the isolated build directory. Install it with sudo pacman -S --needed rsync; sudo is needed to write /usr/bin/rsync and update pacman's database. The source copy itself runs without sudo.".into());
    }
    let work = repo.join(".work/arch");
    fs::create_dir_all(&work)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(work.join("build.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another Droidloom package build is already using this checkout".into());
    }
    let snapshot = work.join("source");
    copy_inputs(&repo, &snapshot)?;
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
        let seed_lock_path = seed
            .parent()
            .ok_or("source cache has no parent")?
            .join("aosp-source-update.lock");
        let _seed_lock = if seed_lock_path.exists() {
            let lock = fs::File::open(seed_lock_path)?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
                return Err("the source checkout is being updated; finish that build before seeding this cache".into());
            }
            Some(lock)
        } else {
            None
        };
        let temporary = tempfile::Builder::new()
            .prefix("source-seed-")
            .tempdir_in(&cache)?;
        eprintln!(
            "Seeding sparse sources from {}; mutable working files are copied and immutable Git objects may share storage.",
            seed.display()
        );
        seed_sources(&seed, temporary.path(), Path::new(""))?;
        fs::rename(temporary.path(), &destination)?;
    }
    // Download archives are inputs, not installed runtime artifacts. Copy them only
    // when absent; the existing upstream-input preparation handles their cache.
    fs::create_dir_all(snapshot.join(".work"))?;
    if let Ok(entries) = fs::read_dir(repo.join(".work")) {
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if entry.file_type()?.is_file()
                && ((name.starts_with("aosp_cf_x86_64_") && name.ends_with(".zip"))
                    || (name.starts_with("mesa-") && name.ends_with(".tar.xz")))
            {
                let destination = snapshot.join(".work").join(entry.file_name());
                if !destination.exists() {
                    fs::copy(entry.path(), destination)?;
                }
            }
        }
    }
    let output = repo.join("dist/arch").join(version.directory());
    fs::create_dir_all(&output)?;
    if fs::read_dir(&output)?
        .any(|e| e.is_ok_and(|e| e.path().extension() == Some(OsStr::new("zst"))))
    {
        return Err(format!("{} already contains packages; increase packaging/arch/version.json release or move the previous artifacts before rebuilding", output.display()).into());
    }
    eprintln!(
        "Building {} for x86_64 with {jobs} jobs; two CPUs reserved. No host sudo is used.",
        version.directory()
    );
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let log_path = work.join(format!("build-{timestamp}.log"));
    let log = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&log_path)?;
    eprintln!("Build log: {}", log_path.display());
    run_logged(
        podman(&work)
            .args([
                "build",
                "--tag",
                "localhost/droidloom-arch-builder",
                "--file",
            ])
            .arg(repo.join("packaging/arch/Containerfile"))
            .arg(repo.join("packaging/arch")),
        &log,
    )?;
    // This storage belongs only to this checkout's package workflow. Drop
    // superseded builder layers so dependency fixes do not fill the host disk.
    run(podman(&work).args(["image", "prune", "--force"]))?;
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let mut container = podman(&work);
    container
        .args(["run", "--rm", "--userns=keep-id", "--user"])
        .arg(format!("{uid}:{gid}"))
        .arg("--cpuset-cpus")
        .arg(
            cpus.iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(","),
        )
        .args([
            "--env",
            "DROIDLOOM_COMPILER_CPUS_RESERVED=1",
            "--env",
            "CARGO_TARGET_DIR=/build/driver",
        ])
        .arg("--volume")
        .arg(format!("{}:/source:rw", snapshot.display()))
        .arg("--volume")
        .arg(format!("{}:/build:rw", cache.display()))
        .arg("--volume")
        .arg(format!("{}:/output:rw", output.display()))
        .arg("localhost/droidloom-arch-builder")
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
            "in-container",
            "--jobs",
        ])
        .arg(jobs.to_string());
    if clean {
        container.arg("--clean");
    }
    run_logged(&mut container, &log)?;
    let mut packages = fs::read_dir(&output)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    packages.retain(|p| p.extension() == Some(OsStr::new("zst")));
    packages.sort();
    if packages.len() != 2 {
        return Err("build did not produce both runtime and image packages".into());
    }
    fs::write(
        output.join("build-summary.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version":version,"elapsed_seconds":started.elapsed().as_secs(),"jobs":jobs,
            "packages":packages.iter().map(|p| Ok(serde_json::json!({"file":p.file_name().map(|n| n.to_string_lossy()),"bytes":fs::metadata(p)?.len()}))).collect::<Result<Vec<_>>>()?
        }))?,
    )?;
    println!("Packages ready in {}", output.display());
    println!(
        "Administrator access is needed only to install these files under /usr, install their runtime dependencies, and update /var/lib/pacman:"
    );
    println!(
        "sudo pacman -U {}",
        packages
            .iter()
            .map(|p| quote_path(p))
            .collect::<Vec<_>>()
            .join(" ")
    );
    Ok(())
}
fn quote_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn seed_sources(source: &Path, destination: &Path, relative: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let relative = relative.join(entry.file_name());
        if relative == Path::new("out") || relative == Path::new("vendor/droidloom") {
            continue;
        }
        let target = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            seed_sources(&entry.path(), &target, &relative)?;
        } else if kind.is_symlink() {
            symlink(fs::read_link(entry.path())?, target)?;
        } else if kind.is_file() {
            let parts: Vec<_> = relative.components().collect();
            let immutable_object = parts.windows(3).any(|p| {
                p[0].as_os_str() == ".git"
                    && p[1].as_os_str() == "objects"
                    && p[2].as_os_str().to_str().is_some_and(|s| {
                        s == "pack" || (s.len() == 2 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                    })
            });
            if !immutable_object || fs::hard_link(entry.path(), &target).is_err() {
                fs::copy(entry.path(), &target)?;
            }
            fs::set_permissions(&target, entry.metadata()?.permissions())?;
        }
    }
    Ok(())
}

fn in_container(jobs: usize, clean: bool) -> Result<()> {
    require_user()?;
    let repo = Path::new("/source");
    let work = Path::new("/build");
    let version = Version::read(repo)?;
    let packaging = work.join("makepkg");
    fs::create_dir_all(&packaging)?;
    remove_staging(&packaging.join("src/payload"))?;
    remove_staging(&packaging.join("pkg"))?;
    let recipe = fs::read_to_string(repo.join("packaging/arch/PKGBUILD.in"))?
        .replace("@VERSION@", &version.version)
        .replace("@RELEASE@", &version.release.to_string());
    fs::write(packaging.join("PKGBUILD"), recipe)?;
    let temporary = tempfile::Builder::new()
        .prefix(".packages-")
        .tempdir_in("/output")?;
    let mut command = Command::new("makepkg");
    command
        .current_dir(&packaging)
        .args(["--force", "--noconfirm", "--nocheck"])
        .env("PKGDEST", temporary.path())
        .env("DROIDLOOM_PACKAGE_TOOL", std::env::current_exe()?)
        .env("DROIDLOOM_SOURCE", repo)
        .env("DROIDLOOM_WORK", work)
        .env("DROIDLOOM_JOBS", jobs.to_string())
        .env(
            "DROIDLOOM_CLEAN_PACKAGE_BUILD",
            if clean { "1" } else { "0" },
        );
    if let Err(error) = run(&mut command) {
        cleanup_package_staging(&packaging);
        return Err(error);
    }
    let packages = fs::read_dir(temporary.path())?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|entry| entry.path().extension() == Some(OsStr::new("zst")))
        .collect::<Vec<_>>();
    if packages.len() != 2 {
        return Err("makepkg did not produce the complete package pair".into());
    }
    for entry in packages {
        fs::rename(entry.path(), Path::new("/output").join(entry.file_name()))?;
    }
    cleanup_package_staging(&packaging);
    Ok(())
}

fn cleanup_package_staging(packaging: &Path) {
    for path in [packaging.join("src/payload"), packaging.join("pkg")] {
        if let Err(error) = remove_staging(&path) {
            eprintln!(
                "Temporary staging cleanup failed for {}: {error}",
                path.display()
            );
        }
    }
}

fn remove_staging(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(format!("unexpected staging directory type: {}", path.display()).into());
        }
    }
    // makepkg deliberately leaves some temporary package directories mode 0111.
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_staging(&entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    fs::remove_dir(path)?;
    Ok(())
}
fn compile(source: &Path, work: &Path, destination: &Path, jobs: usize) -> Result<()> {
    require_user()?;
    run(Command::new("cargo")
        .current_dir(source)
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
        .arg(source)
        .arg("package-stage")
        .arg("--work")
        .arg(work)
        .arg("--destination")
        .arg(destination)
        .arg("--jobs")
        .arg(jobs.to_string());
    if std::env::var("DROIDLOOM_CLEAN_PACKAGE_BUILD").as_deref() == Ok("1") {
        command.arg("--clean");
    }
    run(&mut command)
}

fn install_tree(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_dir() {
        return Err(format!("package staging directory missing: {}", source.display()).into());
    }
    fs::create_dir_all(destination)?;
    fs::set_permissions(destination, fs::metadata(source)?.permissions())?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            install_tree(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            symlink(fs::read_link(entry.path())?, target)?;
        } else if kind.is_file() {
            // These files are a fresh, immutable staging copy, separate from
            // compiler outputs. Sharing them with makepkg's temporary tree
            // avoids another multi-gigabyte copy; stripping is disabled.
            if fs::hard_link(entry.path(), &target).is_err() {
                fs::copy(entry.path(), &target)?;
                fs::set_permissions(target, entry.metadata()?.permissions())?;
            }
        } else {
            return Err("package staging contains a non-file object".into());
        }
    }
    Ok(())
}
fn execute() -> Result<()> {
    match Cli::parse().action {
        Action::Build {
            source,
            jobs,
            clean,
            source_cache,
        } => build(source, jobs, clean, source_cache),
        Action::Check { packages, previous } => validation::check(&packages, previous.as_deref()),
        Action::InContainer { jobs, clean } => in_container(jobs, clean),
        Action::Compile {
            source,
            work,
            destination,
            jobs,
        } => compile(&source, &work, &destination, jobs),
        Action::InstallTree {
            source,
            destination,
        } => install_tree(&source, &destination),
    }
}
fn main() {
    if let Err(error) = execute() {
        eprintln!("Droidloom package build failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn package_copy_preserves_executable_permissions_and_relative_links() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("runtime")).unwrap();
        fs::write(source.join("runtime/helper"), b"test fixture").unwrap();
        fs::set_permissions(
            source.join("runtime/helper"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("runtime", source.join("current")).unwrap();
        let target = temp.path().join("package");
        install_tree(&source, &target).unwrap();
        assert_eq!(
            fs::read_link(target.join("current")).unwrap(),
            Path::new("runtime")
        );
        assert_eq!(
            fs::metadata(target.join("runtime/helper"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
    #[test]
    fn printed_paths_cannot_be_interpreted_as_shell_programs() {
        assert_eq!(
            quote_path(Path::new("a'b $(id).pkg.tar.zst")),
            "'a'\\''b $(id).pkg.tar.zst'"
        );
    }
    #[test]
    fn source_seed_never_shares_mutable_working_files_or_old_build_outputs() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("project/.git/objects/ab")).unwrap();
        fs::create_dir_all(source.join("out")).unwrap();
        fs::write(source.join("project/code"), "original").unwrap();
        fs::write(source.join("project/.git/objects/ab/object"), "git object").unwrap();
        fs::write(source.join("out/binary"), "old build").unwrap();
        let destination = temp.path().join("seed");
        seed_sources(&source, &destination, Path::new("")).unwrap();
        fs::write(destination.join("project/code"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(source.join("project/code")).unwrap(),
            "original"
        );
        assert_eq!(
            fs::metadata(source.join("project/.git/objects/ab/object"))
                .unwrap()
                .ino(),
            fs::metadata(destination.join("project/.git/objects/ab/object"))
                .unwrap()
                .ino()
        );
        assert!(!destination.join("out").exists());
    }
}
