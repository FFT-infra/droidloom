//! Command-line entry point for exact-commit sparse AOSP materialization.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use droidloom_source::{build_plan, load_lock, materialize, reconcile, verify_materialized};

#[derive(Debug, Parser)]
#[command(name = "droidloom-source", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate the bounded sparse lock without touching the network.
    VerifyLock {
        /// Sparse source lock to verify.
        #[arg(long, default_value = "android/manifest/m2-sparse-source-lock.json")]
        lock: PathBuf,
    },
    /// Print the exact immutable project plan without fetching it.
    Plan {
        /// Sparse source lock to inspect.
        #[arg(long, default_value = "android/manifest/m2-sparse-source-lock.json")]
        lock: PathBuf,
    },
    /// Fetch exact commits into a new atomically published source directory.
    Materialize {
        /// Sparse source lock to materialize.
        #[arg(long, default_value = "android/manifest/m2-sparse-source-lock.json")]
        lock: PathBuf,
        /// New output directory. Existing paths are refused.
        #[arg(long)]
        output: PathBuf,
    },
    /// Add newly locked projects to an existing verified materialization.
    Reconcile {
        /// Sparse source lock to reconcile against.
        #[arg(long, default_value = "android/manifest/m2-sparse-source-lock.json")]
        lock: PathBuf,
        /// Existing droidloom-source output directory.
        #[arg(long)]
        output: PathBuf,
    },
    /// Verify an existing materialization without fetching or modifying it.
    VerifyMaterialized {
        /// Sparse source lock the materialization must exactly match.
        #[arg(long, default_value = "android/manifest/m2-sparse-source-lock.json")]
        lock: PathBuf,
        /// Existing droidloom-source output directory.
        #[arg(long)]
        output: PathBuf,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloom-source: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::VerifyLock { lock } => {
            let lock = load_lock(&lock)?;
            let plan = build_plan(&lock)?;
            println!(
                "verified {} exact AOSP projects and {} root links at superproject {}",
                plan.projects.len(),
                plan.links.len(),
                plan.superproject_commit
            );
        }
        Command::Plan { lock } => {
            let lock = load_lock(&lock)?;
            println!("{}", serde_json::to_string_pretty(&build_plan(&lock)?)?);
        }
        Command::Materialize { lock, output } => {
            let source_lock = load_lock(&lock)?;
            let manifest = materialize(&lock, &source_lock, &output)?;
            println!(
                "materialized {} exact AOSP projects at {}",
                manifest.plan.projects.len(),
                output.display()
            );
        }
        Command::Reconcile { lock, output } => {
            let source_lock = load_lock(&lock)?;
            let manifest = reconcile(&lock, &source_lock, &output)?;
            println!(
                "reconciled {} exact AOSP projects at {}",
                manifest.plan.projects.len(),
                output.display()
            );
        }
        Command::VerifyMaterialized { lock, output } => {
            let source_lock = load_lock(&lock)?;
            let manifest = verify_materialized(&lock, &source_lock, &output)?;
            println!(
                "verified {} exact AOSP projects at {}",
                manifest.plan.projects.len(),
                output.display()
            );
        }
    }
    Ok(())
}
