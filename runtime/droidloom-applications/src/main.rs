//! User-session Android application catalog and XDG launcher reconciler.

#![forbid(unsafe_code)]

use std::env;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use clap::Parser;
use droidloom_applications::{CatalogPaths, ControlCatalog, reconcile, remove_managed};
use droidloom_supervisor::control::DEFAULT_CONTROL_SOCKET;

const DEFAULT_REFRESH_SECONDS: u64 = 30;

#[derive(Debug, Parser)]
#[command(name = "droidloom-applications", version, about)]
struct Cli {
    /// Remove only entries recorded in Droidloom's catalog; Android need not be running.
    #[arg(long)]
    remove_managed: bool,
    /// Droidloom lifecycle socket.
    #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
    socket: PathBuf,
    /// Android user whose launchable activities are exported.
    #[arg(long, default_value_t = 0)]
    user: u32,
    /// Reconcile once and exit instead of watching for package changes.
    #[arg(long)]
    once: bool,
    /// Package-catalog polling interval. Android is queried only once per interval.
    #[arg(long, default_value_t = DEFAULT_REFRESH_SECONDS, value_parser = clap::value_parser!(u64).range(5..))]
    refresh_seconds: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloom-applications: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let paths = CatalogPaths::from_environment(
        env::var_os("XDG_DATA_HOME"),
        env::var_os("XDG_STATE_HOME"),
        env::var_os("HOME"),
    )?;
    if cli.remove_managed {
        let count = remove_managed(&paths)?;
        println!("Removed {count} Droidloom launcher/icon entries; Android data retained.");
        return Ok(());
    }
    let mut catalog = ControlCatalog::new(cli.socket, cli.user);

    loop {
        match reconcile(&paths, &mut catalog) {
            Ok(summary) => {
                if summary.changed() {
                    eprintln!(
                        "droidloom-applications: {} launchers, {} icons updated, {} stale entries removed",
                        summary.applications, summary.icons_updated, summary.entries_removed
                    );
                }
            }
            Err(error) if !cli.once => {
                // Droidloom is boot-managed independently of this user helper.
                // A temporary Android restart must not erase the last good XDG
                // catalog or turn the helper into a service failure loop.
                eprintln!("droidloom-applications: catalog refresh deferred: {error}");
            }
            Err(error) => return Err(error.into()),
        }
        if cli.once {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(cli.refresh_seconds));
    }
}
