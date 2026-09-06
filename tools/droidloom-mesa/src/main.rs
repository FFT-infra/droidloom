//! Command-line entry point for pinned Mesa source preparation.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(about, version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Verify, extract, patch, and atomically publish a pinned Mesa archive.
    Prepare {
        /// Droidloom source lock containing the Mesa archive identity.
        #[arg(long, default_value = "android/manifest/source-lock.json")]
        lock: PathBuf,
        /// Previously downloaded Mesa release archive.
        #[arg(long)]
        archive: PathBuf,
        /// Directory containing the ordered Droidloom `.patch` files.
        #[arg(long, default_value = "android/mesa/patches")]
        patches: PathBuf,
        /// New or already verified prepared-source directory.
        #[arg(long)]
        output: PathBuf,
    },
}

fn main() {
    let result = match Cli::parse().command {
        Command::Prepare {
            lock,
            archive,
            patches,
            output,
        } => droidloom_mesa::prepare(&lock, &archive, &patches, &output),
    };

    match result {
        Ok(result) => println!(
            "prepared Mesa {} ({}) at {}",
            result.version,
            result.tree_sha256,
            result.output.display()
        ),
        Err(error) => {
            eprintln!("droidloom-mesa: {error}");
            std::process::exit(1);
        }
    }
}
