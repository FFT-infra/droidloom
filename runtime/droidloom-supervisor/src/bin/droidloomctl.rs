//! User-facing control client for the one warm Droidloom Android cell.

#![forbid(unsafe_code)]

use std::{
    env,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand, ValueEnum};
use droidloom_supervisor::control::{
    ControlRequest, DEFAULT_CELL_SPEC, DEFAULT_CONTROL_SOCKET, request, request_with_progress,
};
use droidloom_supervisor::session_binding::{SessionBinding, SessionDirectory, SessionRecord};
use droidloom_window_policy::{
    LogicalSize, SessionMode, WindowPolicyPaths, WindowPolicyStore, WindowPreference,
};

#[derive(Debug, Parser)]
#[command(name = "droidloomctl", version, about)]
struct Cli {
    /// Droidloom lifecycle socket.
    #[arg(long, global = true, default_value = DEFAULT_CONTROL_SOCKET)]
    socket: PathBuf,
    /// Control only the Android cell (internal service/recovery operation).
    #[arg(long, global = true, hide = true)]
    cell: bool,
    /// Emit the complete machine-readable response.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install a standalone APK, updating an existing app while retaining its data.
    Install {
        /// Host path to a standalone APK (split APK bundles are unsupported).
        apk: PathBuf,
        /// Android user identifier.
        #[arg(long, default_value_t = 0)]
        user: u32,
    },
    /// Update a source installation, or show pacman package-update instructions.
    Update {
        /// Discard updater-owned build outputs before rebuilding.
        #[arg(long)]
        clean: bool,
        /// Build and verify without activating the result.
        #[arg(long)]
        build_only: bool,
    },
    /// Start Droidloom, including Android, native windows, and the app catalog.
    Start {
        /// Return after starting the services, without waiting for Android readiness.
        #[arg(long)]
        no_wait: bool,
        /// Window mode: desktop (default) or mobile (fill available space).
        #[arg(long, default_value = "desktop")]
        mode: SessionMode,
        /// Cell specification.
        #[arg(long, default_value = DEFAULT_CELL_SPEC)]
        spec: PathBuf,
    },
    /// Stop Droidloom and release its runtime resources.
    Stop,
    /// Restart the complete Droidloom runtime.
    Restart {
        /// Return after restarting the services, without waiting for Android readiness.
        #[arg(long)]
        no_wait: bool,
        /// Window mode; omitted preserves the current session mode.
        #[arg(long)]
        mode: Option<SessionMode>,
        /// Cell specification.
        #[arg(long, default_value = DEFAULT_CELL_SPEC)]
        spec: PathBuf,
    },
    /// Report whether the cell is running.
    Status,
    /// Connect the host's adb client to the running cell's adbd.
    Adb {
        /// Disconnect instead of connecting.
        #[arg(long)]
        disconnect: bool,
    },
    /// Wait for Android boot, showing progress and a deadline without restarting it.
    Wait,
    /// Show recent Android logs, optionally filtered by an installed app's UID.
    Logs {
        /// Android package, including retained logs after its process exits.
        package: Option<String>,
        /// Number of recent logcat entries (output is also byte bounded).
        #[arg(short = 'n', long, default_value_t = 200, value_parser = clap::value_parser!(u32).range(1..=2000))]
        lines: u32,
        /// Android user whose package UID should be selected.
        #[arg(long, default_value_t = 0)]
        user: u32,
    },
    /// Show the global crash buffer and recorded exits for an optional package.
    Crashes {
        /// Select this package's exit history; crash traces still include all apps.
        package: Option<String>,
        /// Number of recent crash logcat entries (output is also byte bounded).
        #[arg(short = 'n', long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=2000))]
        lines: u32,
    },
    /// Launch one Android application in the boot-managed runtime.
    Launch {
        /// Android package name.
        package: String,
        /// Optional flattened PACKAGE/ACTIVITY component.
        #[arg(long)]
        component: Option<String>,
        /// Restart this app with initial Android task size WIDTHxHEIGHT pixels.
        #[arg(long, value_parser = droidloom_supervisor::control::parse_launch_resolution)]
        resolution: Option<String>,
        /// Android user identifier.
        #[arg(long, default_value_t = 0)]
        user: u32,
        /// Expected cell specification.
        #[arg(long, default_value = DEFAULT_CELL_SPEC)]
        spec: PathBuf,
    },
    /// List Android applications exported to the native launcher.
    Applications {
        /// Android user identifier.
        #[arg(long, default_value_t = 0)]
        user: u32,
    },
    /// Persist Android's built-in-display density, warming the runtime if needed.
    Dpi {
        /// Android density in dots per inch.
        dpi: u32,
        /// Cell specification used when the runtime is cold.
        #[arg(long, default_value = DEFAULT_CELL_SPEC)]
        spec: PathBuf,
    },
    /// Persist how size-less initial Wayland configures are resolved.
    WindowMode {
        /// Mobile work-area or fixed windowed policy.
        mode: WindowMode,
        /// Logical width required by windowed mode.
        #[arg(long)]
        width: Option<u32>,
        /// Logical height required by windowed mode.
        #[arg(long)]
        height: Option<u32>,
        /// Apply only to this Android package instead of changing the default.
        #[arg(long)]
        package: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum WindowMode {
    /// Fill the standard XDG bounds or logical output extent.
    FitOutput,
    /// Use an explicit logical size, constrained by XDG bounds.
    Windowed,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloomctl: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    if !cli.cell && cli.socket == PathBuf::from(DEFAULT_CONTROL_SOCKET) {
        let operation = match &cli.command {
            Command::Start {
                spec,
                mode,
                no_wait,
            } if spec == &PathBuf::from(DEFAULT_CELL_SPEC) => {
                Some(("start", Some(*mode), *no_wait))
            }
            Command::Stop => Some(("stop", None, true)),
            Command::Restart {
                spec,
                mode,
                no_wait,
            } if spec == &PathBuf::from(DEFAULT_CELL_SPEC) => Some(("restart", *mode, *no_wait)),
            _ => None,
        };
        if let Some((operation, mode, no_wait)) = operation {
            return session_lifecycle(operation, cli.json, mode, no_wait);
        }
        if matches!(cli.command, Command::Status) && !cli.socket.exists() {
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "message": "Droidloom is stopped", "state": "stopped"})
                );
            } else {
                println!("Droidloom is stopped");
            }
            return Ok(());
        }
    }
    let mut report = |message: &str| {
        if !cli.json {
            eprintln!("{message}");
        }
    };
    let wait_after_start = !cli.cell
        && matches!(
            cli.command,
            Command::Start { no_wait: false, .. } | Command::Restart { no_wait: false, .. }
        );
    let mut response = if let Command::Install { apk, user } = &cli.command {
        droidloom_supervisor::control::install_apk_with_progress(
            &cli.socket,
            apk,
            *user,
            &mut report,
        )?
    } else {
        let control_request = match cli.command {
            Command::Install { .. } => unreachable!("handled with its APK file descriptor"),
            Command::Update { clean, build_only } => {
                if PathBuf::from("/usr/share/droidloom/package.json").is_file() {
                    return Err("Droidloom is managed by pacman. Install the new package pair with sudo pacman -U, or build it with cargo run --locked -j 1 -p droidloom-package -- build. Sudo is needed only to replace package-owned system files and update pacman's database.".into());
                }
                let mut command = droidloom_cpu_placement::command("/usr/bin/droidloom-update");
                if clean {
                    command.arg("--clean");
                }
                if build_only {
                    command.arg("--build-only");
                }
                let status = command.status()?;
                if !status.success() {
                    return Err(format!("update failed: {status}").into());
                }
                return Ok(());
            }
            Command::Adb { disconnect } => {
                return adb_connection(droidloom_supervisor::CELL_ADB_ENDPOINT, disconnect);
            }
            Command::Start { spec, .. } => ControlRequest::Start { spec },
            Command::Stop => ControlRequest::Stop,
            Command::Restart { spec, .. } => ControlRequest::Restart { spec },
            Command::Status => ControlRequest::Status,
            Command::Wait => ControlRequest::WaitReady,
            Command::Logs {
                package,
                lines,
                user,
            } => ControlRequest::Diagnostics {
                package,
                lines,
                user,
                crashes: false,
            },
            Command::Crashes { package, lines } => ControlRequest::Diagnostics {
                package,
                lines,
                user: 0,
                crashes: true,
            },
            Command::Launch {
                package,
                component,
                resolution,
                user,
                spec,
            } => ControlRequest::Launch {
                spec,
                package,
                component,
                resolution,
                user,
            },
            Command::Applications { user } => ControlRequest::ListApplications { user },
            Command::Dpi { dpi, spec } => ControlRequest::SetDpi { spec, dpi },
            Command::WindowMode {
                mode,
                width,
                height,
                package,
            } => {
                let preference = match (mode, width, height) {
                    (WindowMode::FitOutput, None, None) => WindowPreference::fit_output(),
                    (WindowMode::Windowed, Some(width), Some(height)) => {
                        WindowPreference::windowed(LogicalSize::new(width, height)?)
                    }
                    (WindowMode::FitOutput, _, _) => {
                        return Err("fit-output mode does not accept --width or --height".into());
                    }
                    (WindowMode::Windowed, _, _) => {
                        return Err("windowed mode requires both --width and --height".into());
                    }
                };
                let paths = WindowPolicyPaths::from_environment(
                    env::var_os("XDG_CONFIG_HOME"),
                    env::var_os("XDG_STATE_HOME"),
                    env::var_os("HOME"),
                )?;
                let mut store = WindowPolicyStore::load(paths)?;
                store.set_preference(package.as_deref(), preference)?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "ok": true,
                            "mode": match mode {
                                WindowMode::FitOutput => "fit_output",
                                WindowMode::Windowed => "windowed",
                            },
                            "width": width,
                            "height": height,
                            "package": package,
                        }))?
                    );
                } else if let Some(package) = package {
                    println!("set persistent Droidloom window policy for {package}");
                } else {
                    println!("set persistent default Droidloom window policy");
                }
                return Ok(());
            }
        };
        if cli.cell {
            request(&cli.socket, &control_request)?
        } else {
            request_with_progress(&cli.socket, &control_request, &mut report)?
        }
    };
    if response.ok && wait_after_start {
        response = request_with_progress(&cli.socket, &ControlRequest::WaitReady, &mut report)?;
    }
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if response.ok {
        if let Some(diagnostics) = &response.diagnostics {
            print!("{diagnostics}");
            if !diagnostics.ends_with('\n') {
                println!();
            }
        } else {
            println!("{}", response.message);
        }
        if let Some(launch) = &response.launch
            && !launch.is_empty()
        {
            println!("{launch}");
        }
        if let Some(applications) = &response.applications {
            for application in applications {
                println!("{}\t{}", application.name, application.component);
            }
        }
    }
    if response.ok {
        Ok(())
    } else {
        Err(response.message.into())
    }
}

/// Connect or disconnect the host's adb client, the equivalent of Waydroid's
/// `waydroid adb`. The host client is the only one admitted to the cell's
/// adbd; the supervisor's input filter accepts no other cell traffic. adb
/// itself reports the outcome, including a refused connection.
fn adb_connection(endpoint: &str, disconnect: bool) -> Result<(), Box<dyn std::error::Error>> {
    if disconnect {
        let status = droidloom_cpu_placement::command("adb")
            .args(["disconnect", endpoint])
            .status()?;
        if !status.success() {
            return Err(format!("adb disconnect failed: {status}").into());
        }
        return Ok(());
    }
    let status = droidloom_cpu_placement::command("adb").arg("start-server").status()?;
    if !status.success() {
        return Err(format!("adb start-server failed: {status}").into());
    }
    let status = droidloom_cpu_placement::command("adb")
        .args(["connect", endpoint])
        .status()?;
    if !status.success() {
        return Err(format!("adb connect failed: {status}").into());
    }
    Ok(())
}

fn session_service_active() -> Result<bool, Box<dyn std::error::Error>> {
    let output = droidloom_cpu_placement::command("systemctl")
        .args(["--user", "show", "--property=ActiveState", "--value", "droidloom.service"])
        .output()?;
    if !output.status.success() {
        return Err("could not inspect droidloom.service; no session environment was changed".into());
    }
    match String::from_utf8(output.stdout)?.trim() {
        "inactive" | "failed" => Ok(false),
        "active" | "activating" | "reloading" | "deactivating" | "maintenance" => Ok(true),
        _ => Err("droidloom.service returned an unknown state; no session environment was changed".into()),
    }
}

fn run_session_service_command(
    operation: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !json {
        eprintln!("Waiting for Droidloom services to {operation} (up to 160 seconds)...");
    }
    let mut child = droidloom_cpu_placement::command("systemctl")
        .args(["--user", operation, "droidloom.service"])
        .spawn()?;
    let started = Instant::now();
    let mut next_update = Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? { break status; }
        if started.elapsed() >= Duration::from_secs(160) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Droidloom services did not {operation} within 160 seconds. The systemd job may still be running. Check the logs with:\n  journalctl --user -b -u droidloom.service -n 100 --no-pager\n  journalctl -b -u droidloomd.service -n 200 --no-pager").into());
        }
        if started.elapsed() >= next_update {
            if !json {
                eprintln!("Waiting for Droidloom services to {operation} ({} seconds elapsed; {} seconds remaining)...",
                    started.elapsed().as_secs(), 160 - started.elapsed().as_secs());
            }
            next_update += Duration::from_secs(10);
        }
        thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        return Err(format!("{operation} failed; see journalctl --user -u droidloom.service").into());
    }
    Ok(())
}

fn session_lifecycle(
    operation: &str,
    json: bool,
    requested_mode: Option<SessionMode>,
    no_wait: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = SessionDirectory::from_environment()?;
    let _transaction = directory.lock_commands()?;
    if operation == "stop" {
        // Stop is explicit recovery, so even an invalid legacy record must not
        // prevent it. Internal --cell operations never enter this path.
        run_session_service_command("stop", json)?;
    } else {
        let previous = directory.read_record()?;
        let mode = requested_mode.unwrap_or_else(|| {
            previous.as_ref().map_or(SessionMode::Desktop, SessionRecord::mode)
        });
        let binding = SessionBinding::from_environment(directory.runtime(), mode)?;
        let active = session_service_active()?;
        let owner_locked = directory.owner_locked()?;
        let previous_binding = match &previous {
            Some(SessionRecord::Bound(value)) => Some(value),
            _ => None,
        };
        if owner_locked {
            let owner = previous_binding.ok_or(
                "Droidloom has a live presenter without a bound session record; stop that session explicitly",
            )?;
            if !owner.same_owner(&binding) {
                return Err(format!(
                    "Droidloom belongs to another graphical session ({}; compositor {}); its environment and applications were left unchanged",
                    owner.compositor.socket.display(), owner.compositor.pid,
                ).into());
            }
            if !active {
                return Err("a presenter outside droidloom.service owns the runtime; stop it explicitly before starting the service".into());
            }
        } else if active {
            return Err("a legacy or starting Droidloom service has no presenter ownership lease; explicitly stop it before starting a bound session".into());
        }
        let unchanged = active && operation == "start" && previous_binding.is_some_and(|old| {
            old.mode == binding.mode && old.host_navigation == binding.host_navigation
        });
        if !unchanged {
            // Ownership rejection precedes package preparation, reload, env
            // writes and any command that could stop another desktop's apps.
            if PathBuf::from("/usr/share/droidloom/package.json").is_file() {
                let status = droidloom_cpu_placement::command("/usr/lib/droidloom/droidloom-package-helper")
                    .arg("prepare").status()?;
                if !status.success() {
                    return Err("Droidloom setup did not complete; the runtime was not started".into());
                }
                let status = droidloom_cpu_placement::command("systemctl")
                    .args(["--user", "daemon-reload"]).status()?;
                if !status.success() {
                    return Err("could not reload the installed Droidloom user service".into());
                }
            }
            if active {
                if !json {
                    eprintln!("Restarting Droidloom closes Android windows; the graphical desktop remains running.");
                }
                run_session_service_command("stop", json)?;
                if directory.owner_locked()? {
                    return Err("Droidloom presenter lease remained held after the service stopped".into());
                }
            }
            directory.write_binding(&binding)?;
            run_session_service_command("start", json)?;
        }
    }
    let message = if operation == "stop" {
        "Droidloom is stopped"
    } else if no_wait {
        "Droidloom services are running; Android readiness has not been checked. Use `droidloomctl wait` to follow boot progress."
    } else {
        let response = request_with_progress(
            &PathBuf::from(DEFAULT_CONTROL_SOCKET),
            &ControlRequest::WaitReady,
            &mut |message| {
                if !json {
                    eprintln!("{message}");
                }
            },
        )?;
        if !response.ok {
            return Err(response.message.into());
        }
        "Droidloom is running; Android is ready"
    };
    if json {
        println!(
            "{}",
            serde_json::json!({"ok": true, "message": message, "state": if operation == "stop" { "stopped" } else { "running" }})
        );
    } else {
        println!("{message}");
    }
    Ok(())
}
