//! Command-line entry point for verified Android base-image preparation.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use droidloom_image::{load_source_lock, prepare_base, verify_elf_closure};

#[derive(Debug, Parser)]
#[command(name = "droidloom-image", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Verify and extract only reusable partitions from the pinned CI bundle.
    PrepareBase {
        /// Downloaded official Android CI image zip.
        #[arg(long)]
        archive: PathBuf,
        /// Source lock containing the selected build and checksums.
        #[arg(long, default_value = "android/manifest/source-lock.json")]
        source_lock: PathBuf,
        /// New destination directory. Existing paths are refused.
        #[arg(long)]
        output: PathBuf,
    },
    /// Verify the complete vendor ELF dependency closure against a base tree.
    VerifyElfClosure {
        /// Extracted or build-staged vendor partition root.
        #[arg(long)]
        vendor: PathBuf,
        /// Extracted `/system` partition root (the directory containing `bin` and `lib64`).
        #[arg(long)]
        system: PathBuf,
        /// Extracted `/system_ext` partition root.
        #[arg(long)]
        system_ext: PathBuf,
        /// Extracted `/product` partition root.
        #[arg(long)]
        product: PathBuf,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloom-image: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::PrepareBase {
            archive,
            source_lock,
            output,
        } => {
            let lock = load_source_lock(&source_lock)?;
            let manifest = prepare_base(&archive, &output, &lock.aosp_ci_base)?;
            println!(
                "prepared Android {} build {} ({}) at {}",
                manifest.android_release,
                manifest.build_id,
                manifest.build_number,
                output.display()
            );
        }
        Command::VerifyElfClosure {
            vendor,
            system,
            system_ext,
            product,
        } => {
            let report = verify_elf_closure(&vendor, &system, &system_ext, &product)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}
