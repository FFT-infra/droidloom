//! Summarize actual sampled stage durations; never infer input/display latency.
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, BufRead},
    path::PathBuf,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(about = "Summarize and compare opt-in Droidloom frame-stage timings")]
struct Args {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Read text logcat/journal captures and write a reusable JSON report.
    Summarize {
        #[arg(required = true)]
        logs: Vec<PathBuf>,
        #[arg(long)]
        output: PathBuf,
        /// Describe the build, workload, refresh rate and capture conditions.
        #[arg(long)]
        label: String,
    },
    /// Compare matching stage, outcome and layer-count groups (milliseconds).
    Compare {
        baseline: PathBuf,
        candidate: PathBuf,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Distribution {
    mean_ns: f64,
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    max_ns: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Row {
    stage: String,
    outcome: u64,
    layers: u64,
    samples: usize,
    wall: Distribution,
    cpu: Distribution,
}

impl Row {
    fn key(&self) -> (&str, u64, u64) {
        (&self.stage, self.outcome, self.layers)
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
struct Loss {
    dropped_records: u64,
    sequence_gaps: u64,
    capped_processes: usize,
    duplicate_records: u64,
    malformed_timing_records: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Report {
    schema: u32,
    label: String,
    sources: Vec<PathBuf>,
    loss: Loss,
    timings: Vec<Row>,
}

#[derive(Default)]
struct Process {
    sequences: BTreeSet<u64>,
    dropped: u64,
    capped: bool,
}

fn distribution(mut values: Vec<u64>) -> Distribution {
    values.sort_unstable();
    let percentile = |p: usize| values[(values.len() * p).div_ceil(100).saturating_sub(1)];
    Distribution {
        mean_ns: values.iter().map(|&n| n as f64).sum::<f64>() / values.len() as f64,
        p50_ns: percentile(50),
        p95_ns: percentile(95),
        p99_ns: percentile(99),
        max_ns: *values.last().unwrap(),
    }
}

#[derive(Default)]
struct Capture {
    groups: BTreeMap<(String, u64, u64), Vec<(u64, u64)>>,
    processes: BTreeMap<(usize, u64), Process>,
    loss: Loss,
    records: usize,
}

impl Capture {
    fn line(&mut self, source: usize, line: &str) -> Result<()> {
        let Some((_, line)) = line.split_once("Droidloom frame trace: ") else {
            return Ok(());
        };
        self.records += 1;
        if self.records > 2_000_000 {
            return Err("capture exceeds two million trace records; split it into runs".into());
        }
        let fields: BTreeMap<_, _> = line
            .split_whitespace()
            .filter_map(|v| v.split_once('='))
            .collect();
        let number = |key| fields.get(key).and_then(|value| value.parse::<u64>().ok());
        let timing = fields
            .get("stage")
            .is_some_and(|s| s.starts_with("timing_"));
        if let (Some(pid), Some(sequence)) = (number("pid"), number("seq")) {
            let process = self.processes.entry((source, pid)).or_default();
            process.dropped = process.dropped.max(number("dropped_total").unwrap_or(0));
            process.capped |= sequence >= 65_536;
            if !process.sequences.insert(sequence) {
                self.loss.duplicate_records += 1;
                return Ok(());
            }
        } else if timing {
            self.loss.malformed_timing_records += 1;
            return Ok(());
        }
        if !timing {
            return Ok(());
        }
        let (Some(wall), Some(cpu), Some(layers), Some(outcome), Some(start)) = (
            number("wall_ns"),
            number("cpu_ns"),
            number("layers"),
            number("outcome"),
            number("start_ns"),
        ) else {
            self.loss.malformed_timing_records += 1;
            return Ok(());
        };
        if start == 0 {
            self.loss.malformed_timing_records += 1;
            return Ok(());
        }
        self.groups
            .entry((fields["stage"].to_string(), outcome, layers))
            .or_default()
            .push((wall, cpu));
        Ok(())
    }

    fn report(mut self, sources: Vec<PathBuf>, label: String) -> Result<Report> {
        if self.groups.is_empty() {
            return Err(
                "no frame-stage timing records found; enable tracing in the instrumented build"
                    .into(),
            );
        }
        for process in self.processes.values() {
            self.loss.dropped_records += process.dropped;
            self.loss.capped_processes += usize::from(process.capped);
            let mut previous = None;
            for &seq in &process.sequences {
                if let Some(prev) = previous {
                    self.loss.sequence_gaps += seq - prev - 1;
                }
                previous = Some(seq);
            }
        }
        let timings = self
            .groups
            .into_iter()
            .map(|((stage, outcome, layers), samples)| Row {
                stage,
                outcome,
                layers,
                samples: samples.len(),
                wall: distribution(samples.iter().map(|v| v.0).collect()),
                cpu: distribution(samples.iter().map(|v| v.1).collect()),
            })
            .collect();
        Ok(Report {
            schema: 1,
            label,
            sources,
            loss: self.loss,
            timings,
        })
    }
}

fn read_report(path: &PathBuf) -> Result<Report> {
    let report: Report = serde_json::from_slice(&fs::read(path)?)?;
    if report.schema != 1 {
        return Err("unsupported timing report schema".into());
    }
    Ok(report)
}

fn print_loss(report: &Report) {
    eprintln!(
        "{}: dropped={} gaps={} capped_processes={} duplicates={} malformed={}",
        report.label,
        report.loss.dropped_records,
        report.loss.sequence_gaps,
        report.loss.capped_processes,
        report.loss.duplicate_records,
        report.loss.malformed_timing_records
    );
}

fn run() -> Result<()> {
    match Args::parse().command {
        Action::Summarize {
            logs,
            output,
            label,
        } => {
            let mut capture = Capture::default();
            for (source, path) in logs.iter().enumerate() {
                for line in io::BufReader::new(fs::File::open(path)?).lines() {
                    capture.line(source, &line?)?;
                }
            }
            let report = capture.report(logs, label)?;
            println!(
                "stage outcome layers samples | wall mean/p50/p95/p99/max ms | CPU mean/p50/p95/p99/max ms"
            );
            for r in &report.timings {
                let w = &r.wall;
                let c = &r.cpu;
                println!(
                    "{} {} {} {} | {:.4}/{:.4}/{:.4}/{:.4}/{:.4} | {:.4}/{:.4}/{:.4}/{:.4}/{:.4}",
                    r.stage,
                    r.outcome,
                    r.layers,
                    r.samples,
                    w.mean_ns / 1e6,
                    w.p50_ns as f64 / 1e6,
                    w.p95_ns as f64 / 1e6,
                    w.p99_ns as f64 / 1e6,
                    w.max_ns as f64 / 1e6,
                    c.mean_ns / 1e6,
                    c.p50_ns as f64 / 1e6,
                    c.p95_ns as f64 / 1e6,
                    c.p99_ns as f64 / 1e6,
                    c.max_ns as f64 / 1e6
                );
            }
            print_loss(&report);
            fs::write(output, serde_json::to_vec_pretty(&report)?)?;
        }
        Action::Compare {
            baseline,
            candidate,
        } => {
            let baseline = read_report(&baseline)?;
            let candidate = read_report(&candidate)?;
            print_loss(&baseline);
            print_loss(&candidate);
            println!(
                "stage outcome layers samples(before/after) | wall p50 before→after, Δp95 ms | CPU mean before→after, Δp95 ms"
            );
            let before: BTreeMap<_, _> = baseline.timings.iter().map(|r| (r.key(), r)).collect();
            let after: BTreeMap<_, _> = candidate.timings.iter().map(|r| (r.key(), r)).collect();
            let keys: BTreeSet<_> = before.keys().chain(after.keys()).copied().collect();
            for key in keys {
                match (before.get(&key), after.get(&key)) {
                    (Some(a), Some(b)) => println!(
                        "{} {} {} {}/{} | {:.4}→{:.4}, {:+.4} | {:.4}→{:.4}, {:+.4}",
                        key.0,
                        key.1,
                        key.2,
                        a.samples,
                        b.samples,
                        a.wall.p50_ns as f64 / 1e6,
                        b.wall.p50_ns as f64 / 1e6,
                        (b.wall.p95_ns as f64 - a.wall.p95_ns as f64) / 1e6,
                        a.cpu.mean_ns / 1e6,
                        b.cpu.mean_ns / 1e6,
                        (b.cpu.p95_ns as f64 - a.cpu.p95_ns as f64) / 1e6
                    ),
                    _ => println!(
                        "{} {} {}: missing from {}",
                        key.0,
                        key.1,
                        key.2,
                        if before.contains_key(&key) {
                            "candidate"
                        } else {
                            "baseline"
                        }
                    ),
                }
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn line(seq: u64, layers: u64, outcome: u64, wall: &str) -> String {
        format!(
            "logcat prefix Droidloom frame trace: pid=42 seq={seq} stage=timing_sf_snapshot_copy object=7 frame=1 buffer=0 observed_ns=100 start_ns=10 wall_ns={wall} cpu_ns=2 layers={layers} outcome={outcome} dropped_total=3"
        )
    }
    #[test]
    fn tails_are_nearest_rank_and_groups_keep_failures_and_layer_counts_separate() {
        let d = distribution((1..=100).collect());
        assert_eq!((d.p50_ns, d.p95_ns, d.p99_ns, d.max_ns), (50, 95, 99, 100));
        assert_eq!(distribution(vec![9]).p99_ns, 9);
        let mut capture = Capture::default();
        for (seq, layers, result) in [(1, 1, 1), (3, 1, 0), (4, 2, 1)] {
            capture.line(0, &line(seq, layers, result, "10")).unwrap();
        }
        capture.line(0, &line(4, 2, 1, "10")).unwrap();
        capture.line(0, &line(5, 2, 1, "invalid")).unwrap();
        capture.line(0, "unrelated log").unwrap();
        let r = capture.report(vec![], "fixture".into()).unwrap();
        assert_eq!(r.timings.len(), 3);
        assert_eq!(r.loss.sequence_gaps, 1);
        assert_eq!(r.loss.dropped_records, 3);
        assert_eq!(r.loss.duplicate_records, 1);
        assert_eq!(r.loss.malformed_timing_records, 1);
    }
    #[test]
    fn cap_and_loss_are_reported_and_empty_capture_fails() {
        assert!(Capture::default().report(vec![], "empty".into()).is_err());
        let mut c = Capture::default();
        c.line(0, &line(65_536, 1, 1, "0")).unwrap();
        c.line(1, &line(65_536, 1, 1, "0")).unwrap();
        let r = c.report(vec![], "fixture".into()).unwrap();
        assert_eq!(r.loss.capped_processes, 2);
        assert_eq!(r.loss.sequence_gaps, 0);
        assert_eq!(r.timings[0].samples, 2);
    }
}
