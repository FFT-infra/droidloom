//! Privileged single-cell Droidloom lifecycle daemon.

#![deny(unsafe_op_in_unsafe_fn)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;
use droidloom_supervisor::control::{
    DEFAULT_CONTROL_SOCKET, DEFAULT_SPEC_DIRECTORY, DEFAULT_SUPERVISOR, DaemonConfig, serve,
};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Parser)]
#[command(name = "droidloomd", version, about)]
struct Cli {
    /// Root-owned local lifecycle socket.
    #[arg(long, default_value = DEFAULT_CONTROL_SOCKET)]
    socket: PathBuf,
    /// Small supervisor executable providing the private namespace entry.
    #[arg(long, default_value = DEFAULT_SUPERVISOR)]
    supervisor: PathBuf,
    /// Root-owned cell-specification directory trusted for user requests.
    #[arg(long, default_value = DEFAULT_SPEC_DIRECTORY)]
    spec_directory: PathBuf,
}

extern "C" fn request_shutdown(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloomd: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    droidloom_cpu_placement::initialize(droidloom_cpu_placement::Role::Background);
    install_signal_handlers()?;
    let cli = Cli::parse();
    let config = DaemonConfig {
        socket: cli.socket,
        supervisor: cli.supervisor,
        spec_directory: cli.spec_directory,
    };
    serve(&config, &SHUTDOWN)?;
    Ok(())
}

fn install_signal_handlers() -> Result<(), std::io::Error> {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let previous =
            unsafe { libc::signal(signal, request_shutdown as *const () as libc::sighandler_t) };
        if previous == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
