//! One complete, verified Droidloom build and activation transaction.
mod android;
mod assemble;
mod bundle;
mod dependencies;
mod install;
mod licenses;
mod package;
mod python;
mod util;
use clap::{Parser, Subcommand};
use std::{fs, path::PathBuf, process::Command};
use util::*;
#[derive(Parser)]
#[command(
    version,
    about = "Build, verify, install and restart all Droidloom components"
)]
struct Args {
    /// Droidloom checkout (remembered automatically after installation).
    #[arg(long, global = true)]
    source: Option<PathBuf>,
    /// Delete this updater's build outputs before compiling.
    #[arg(long)]
    clean: bool,
    #[arg(long, hide = true)]
    bootstrapped: bool,
    /// Build and verify a complete bundle without installing it.
    #[arg(long)]
    build_only: bool,
    /// Maximum parallel jobs, capped at available CPUs minus two.
    #[arg(short = 'j', long)]
    jobs: Option<usize>,
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(Subcommand)]
enum Action {
    /// Compile and stage a portable pacman payload; does not install or start services.
    PackageStage {
        #[arg(long)]
        work: PathBuf,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        clean: bool,
        #[arg(long)]
        jobs: usize,
    },
    /// Show the complete build plan without changing anything.
    Plan,
    /// Verify an already built bundle.
    Verify { payload: PathBuf },
    /// Privileged activation worker; normally invoked automatically through polkit.
    #[command(hide = true)]
    Apply {
        payload: PathBuf,
        #[arg(long)]
        uid: u32,
    },
    #[command(hide = true)]
    Recover {
        #[arg(long)]
        uid: u32,
    },
}
fn repository(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = if let Some(p) = explicit {
        p
    } else {
        let current = std::env::current_dir()?;
        if let Some(p) = current
            .ancestors()
            .find(|p| p.join("tools/droidloom-update/Cargo.toml").exists())
        {
            p.to_owned()
        } else {
            let config: serde_json::Value = serde_json::from_slice(
                &fs::read("/usr/lib/droidloom/active/etc/droidloom/update.json")
                    .map_err(|_| "run the initial update inside the Droidloom checkout")?,
            )?;
            PathBuf::from(
                config["source"]
                    .as_str()
                    .ok_or("invalid updater configuration")?,
            )
        }
    }
    .canonicalize()?;
    if !path.join("tools/droidloom-update/Cargo.toml").is_file() {
        return fail("not a Droidloom checkout");
    }
    Ok(path)
}
fn execute(args: Args) -> Result<()> {
    match &args.command {
        Some(Action::PackageStage { work, destination, clean, jobs }) => {
            return package::stage(&repository(args.source.clone())?, work, destination, *clean, *jobs);
        }
        Some(Action::Recover { uid }) => return install::recover(*uid),
        Some(Action::Apply { payload, uid }) => {
            return install::apply(&payload.canonicalize()?, *uid);
        }
        Some(Action::Verify { payload }) => {
            let m = bundle::verify(payload)?;
            println!(
                "Verified {} bundle, input ABI {}, source {}",
                m.architecture, m.input_abi, m.source_identity
            );
            return Ok(());
        }
        _ => {}
    }
    if args.command.is_none() && !args.build_only && std::path::Path::new("/usr/share/droidloom/package.json").exists() {
        return fail("Droidloom is managed by pacman. Build packages with cargo run --locked -j 1 -p droidloom-package -- build, then install the printed package pair with sudo pacman -U. Sudo is needed to replace package-owned system files and update pacman's database.");
    }
    let repo = repository(args.source)?;
    let arch = std::env::consts::ARCH;
    if arch != "x86_64" {
        return fail("the source builder currently requires an x86_64 Linux build host");
    }
    let product = "droidloom_x86_64";
    let work = repo.join(".work/update");
    let source = repo.join(".work/aosp-m2-source");
    let out = work.join("android-out");
    let cargo = work.join("cargo");
    if matches!(args.command, Some(Action::Plan)) {
        println!(
            "Source: {}\nCache: {}\nAndroid product: {product}\nHost: supervisor, ctl, daemon, Wayland, applications, doctor, updater\nAndroid: {}\nPinned inputs: system, system_ext, product, Mesa\nActivation: verify -> stage -> stop Droidloom -> switch complete release -> start -> check Android catalog\nFailure: restore previous release\nBoot autostart: disabled",
            repo.display(),
            work.display(),
            android::TARGETS.join(", ")
        );
        return Ok(());
    }
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        return fail(
            "run as the desktop user; administrator authentication is requested only for installation",
        );
    }
    let capacity = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(1);
    let jobs = args.jobs.unwrap_or(capacity).min(capacity);
    if jobs == 0 {
        return fail("jobs must be positive");
    }
    let _lock = Lock::acquire(&work.join("update.lock"))?;
    let _source_lock = Lock::acquire(&repo.join(".work/aosp-source-update.lock"))?;
    dependencies::ensure()?;
    if work.join("source-projection.json").exists() {
        android::recover(&work.join("source-projection.json"), &source)?;
    }
    if args.clean {
        for path in [&out, &cargo, &work.join("mesa-tools"), &work.join("ccache")] {
            if path.exists() {
                fs::remove_dir_all(path)?;
            }
        }
    }
    let identity = bundle::source_identity(&repo)?;
    eprintln!("Building complete Droidloom release from {identity}");
    eprintln!("Compiler budget: {jobs} parallel jobs; two logical CPUs reserved");
    let mut host_build = Command::new("cargo");
    host_build
        .current_dir(&repo)
        .env("CARGO_TARGET_DIR", &cargo)
        .args(["build", "--locked", "--release"])
        .arg(format!("-j{jobs}"));
    for package in assemble::HOST_PACKAGES {
        host_build.arg("-p").arg(package);
    }
    run_build(&mut host_build)?;
    let fresh = cargo.join("release/droidloom-update");
    if !args.bootstrapped {
        // The updater is part of the build too. Use its new orchestration in this update.
        // Release locks before exec so the replacement can acquire them normally.
        drop(_source_lock);
        drop(_lock);
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(fresh);
        command
            .arg("--bootstrapped")
            .arg("--source")
            .arg(&repo)
            .arg("--jobs")
            .arg(jobs.to_string());
        if args.build_only {
            command.arg("--build-only");
        }
        // --clean was already applied; repeating it would cause an endless bootstrap.
        return Err(command.exec().into());
    }
    if !args.build_only && std::path::Path::new("/usr/lib/droidloom/transaction.json").exists() {
        run(Command::new("pkexec")
            .arg(&fresh)
            .arg("recover")
            .arg("--uid")
            .arg(uid.to_string()))?;
    }
    let base = assemble::prepare_inputs(&repo, &work, arch)?;
    python::prepare(&repo, &work)?;
    assemble::mesa_tools(&work, jobs)?;
    android::build(&repo, &source, &out, &work, product, jobs)?;
    let staging = tempfile::Builder::new()
        .prefix("bundle-")
        .tempdir_in(&work)?;
    assemble::assemble(
        &repo,
        &work,
        &base,
        &out.join("target/product").join(product),
        &cargo.join("release"),
        staging.path(),
        uid,
    )?;
    let id = bundle::seal(staging.path(), identity)?;
    bundle::verify(staging.path())?;
    let payload = work.join(format!("release-{id}"));
    if payload.exists() {
        bundle::verify(&payload)?;
    } else {
        fs::rename(staging.path(), &payload)?;
    }
    println!("Verified bundle: {}", payload.display());
    if !args.build_only {
        run(Command::new("pkexec")
            .arg(payload.join("usr/bin/droidloom-update"))
            .arg("apply")
            .arg(&payload)
            .arg("--uid")
            .arg(uid.to_string()))?;
    }
    Ok(())
}
fn main() {
    if let Err(error) = execute(Args::parse()) {
        eprintln!("Droidloom update failed: {error}");
        std::process::exit(1);
    }
}
