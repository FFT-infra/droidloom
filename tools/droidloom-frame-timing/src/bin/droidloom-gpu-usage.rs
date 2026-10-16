//! Offline-friendly, bounded GPU accounting. Never changes the graphics session.
#[path = "../gpu.rs"]
mod gpu;
use clap::{Parser, Subcommand};
use gpu::Result;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
#[command(about = "Read DRM per-client engine busy counters and memory; no driver changes")]
struct Args {
    #[command(subcommand)]
    action: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Find clients in the current host PID namespace (administrator access may be needed).
    Probe {
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Capture explicitly selected host PIDs; files are saved in a new directory.
    Capture {
        /// Repeat LABEL=PID for each process; shared clients are reported once.
        #[arg(long = "process", required = true)]
        processes: Vec<String>,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        seconds: u64,
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(100..=10000))]
        interval_ms: u64,
        #[arg(long)]
        label: String,
        #[arg(long)]
        output: PathBuf,
        /// End early and write the report when this initially absent file appears.
        #[arg(long)]
        stop_file: Option<PathBuf>,
    },
}

fn json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    Ok(())
}

fn summary(samples: &[gpu::Snapshot], rows: &[gpu::EngineUsage]) -> String {
    let mut text = String::from(
        "# GPU client accounting\n\nDriver-reported engine busy time, not fence-wait time, shader occupancy, power or end-to-end latency. Clients are deduplicated by driver, device and DRM client ID across all selected handles/processes. Shared-client owners cannot be attributed separately. Engine percentages may overlap across clients/engines and must not be summed as total GPU utilization. Memory may be shared between clients; do not sum it as unique VRAM.\n\n| Owners (host PID) | Driver/device/client | Engine | Busy ms | Covered s | Engine capacity % | Intervals | Counter regressions | Discontinuities |\n|---|---|---|---:|---:|---:|---:|---:|---:|\n",
    );
    for row in rows {
        let owners = row
            .owners
            .iter()
            .map(|p| {
                format!(
                    "{} ({})",
                    p.label.replace('|', "/").replace(['\n', '\r'], " "),
                    p.pid
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let percent = row
            .capacity_percent
            .map(|v| format!("{v:.4}"))
            .unwrap_or_else(|| "unavailable".into());
        text.push_str(&format!(
            "| {owners} | {} / {} / {} | {} | {:.3} | {:.3} | {percent} | {} | {} | {} |\n",
            row.client.driver,
            row.client.device.as_deref().unwrap_or("global client IDs"),
            row.client.client_id,
            row.engine,
            row.busy_ns as f64 / 1e6,
            row.covered_ns as f64 / 1e9,
            row.intervals,
            row.regressions,
            row.discontinuities
        ));
    }
    let clients: BTreeMap<_, _> = samples
        .iter()
        .flat_map(|s| &s.clients)
        .map(|c| (&c.key, c))
        .collect();
    let unavailable: Vec<_> = clients
        .values()
        .filter(|c| !rows.iter().any(|r| r.client == c.key))
        .collect();
    if !unavailable.is_empty() {
        text.push_str("\nClients without nanosecond engine counters (GPU usage unavailable):\n\n");
        for client in unavailable {
            text.push_str(&format!(
                "- {} / {} / client {}: {}\n",
                client.key.driver,
                client.key.device.as_deref().unwrap_or("global client IDs"),
                client.key.client_id,
                client
                    .owners
                    .iter()
                    .map(|p| format!("{} ({})", p.label.replace(['\n', '\r'], " "), p.pid))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let max_scan = samples.iter().map(|s| s.scan_ns).max().unwrap_or(0);
    let issues: usize = samples.iter().map(|s| s.issues.len()).sum();
    text.push_str(&format!("\n{} snapshots; max scan {:.3} ms; {} read/identity/unsupported-accounting issues. Raw counters, per-client memory and issues are retained in samples.jsonl and report.json. Missing counters are unavailable, not zero. New or missing clients do not contribute fabricated intervals. Nonmonotonic counters retain their high-water mark until they catch up, as required by the kernel interface. Capacity defaults to one when the driver omits it.\n", samples.len(), max_scan as f64 / 1e6, issues));
    text
}

fn run() -> Result<()> {
    let root = Path::new("/proc");
    match Args::parse().action {
        Action::Probe { output } => {
            let (processes, inaccessible) = gpu::discover(root)?;
            let snap = gpu::snapshot(root, &processes, Instant::now());
            for c in &snap.clients {
                println!(
                    "{} {} client={} engines={:?} owners={:?} unique_handles={}",
                    c.key.driver,
                    c.key.device.as_deref().unwrap_or("global"),
                    c.key.client_id,
                    c.engine_ns.keys().collect::<Vec<_>>(),
                    c.owners
                        .iter()
                        .map(|p| (&p.label, p.pid))
                        .collect::<Vec<_>>(),
                    c.handles.len()
                );
            }
            eprintln!(
                "{} clients; {} issues; {} inaccessible/exited processes",
                snap.clients.len(),
                snap.issues.len(),
                inaccessible
            );
            if let Some(path) = output {
                json_file(
                    &path,
                    &serde_json::json!({"schema": 1, "inaccessible_processes": inaccessible, "snapshot": snap}),
                )?;
            }
        }
        Action::Capture {
            processes,
            seconds,
            interval_ms,
            label,
            output,
            stop_file,
        } => {
            if let Some(path) = &stop_file {
                if path.try_exists()? {
                    return Err("stop file already exists; choose a fresh capture marker".into());
                }
            }
            let mut selected = Vec::new();
            for item in processes {
                let (label, pid) = item.rsplit_once('=').ok_or("process must be LABEL=PID")?;
                if label.is_empty() {
                    return Err("empty process label".into());
                }
                let p = gpu::process(root, pid.parse()?, label.into())?;
                if selected.iter().any(|old: &gpu::Process| old.pid == p.pid) {
                    return Err("duplicate process PID".into());
                }
                selected.push(p);
            }
            let boot = fs::read_to_string(root.join("sys/kernel/random/boot_id"))?;
            if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            fs::create_dir(&output)?; // retain existing captures
            json_file(
                &output.join("build.json"),
                &serde_json::json!({"schema": 1, "label": label, "boot_id": boot.trim(), "started_unix_ns": SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos().to_string(), "processes": selected, "seconds": seconds, "interval_ms": interval_ms, "stop_file": stop_file, "method": "Read-only DRM fdinfo, no frame tracing or runtime changes", "tool_version": env!("CARGO_PKG_VERSION")}),
            )?;
            let mut raw = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output.join("samples.jsonl"))?;
            let epoch = Instant::now();
            let duration = Duration::from_secs(seconds);
            let interval = Duration::from_millis(interval_ms);
            let mut samples = Vec::new();
            loop {
                if fs::read_to_string(root.join("sys/kernel/random/boot_id"))? != boot {
                    return Err("boot identity changed; raw partial capture retained".into());
                }
                let sample = gpu::snapshot(root, &selected, epoch);
                serde_json::to_writer(&mut raw, &sample)?;
                writeln!(raw)?;
                raw.flush()?;
                samples.push(sample);
                let stopped = match &stop_file {
                    Some(path) => path.try_exists()?,
                    None => false,
                };
                if epoch.elapsed() >= duration || stopped {
                    break;
                }
                // Deadline scheduling avoids accumulating scan time each interval.
                let next = (interval * (samples.len() as u32)).min(duration);
                if let Some(wait) = next.checked_sub(epoch.elapsed()) {
                    thread::sleep(wait);
                }
            }
            let rows = gpu::summarize(&samples);
            let text = summary(&samples, &rows);
            json_file(
                &output.join("report.json"),
                &serde_json::json!({"schema": 1, "label": label, "boot_id": boot.trim(), "engines": rows, "samples": samples}),
            )?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output.join("summary.md"))?;
            file.write_all(text.as_bytes())?;
            print!("{text}");
            if samples.iter().all(|s| s.clients.is_empty()) {
                return Err(
                    "no readable DRM clients; inspect issues and process permissions".into(),
                );
            }
        }
    }
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
