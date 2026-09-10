//! Run the existing Digitalis host workloads; never install or activate a runtime.
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    error::Error,
    fs::{self, File},
    io::Read,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const CASES: [(&str, &str); 7] = [
    ("Int", "int"),
    ("Branch", "branch"),
    ("Call", "call"),
    ("Neon", "neon"),
    ("Fp", "fp"),
    ("Mem", "mem"),
    ("Syscall", "syscall"),
];

#[derive(Parser)]
#[command(about = "Stress Digitalis correctness, measure guest loops, compare saved runs")]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Run correctness first, then benchmarks; creates a new output directory.
    Run(RunArgs),
    /// Compare compatible result.json files. Positive time delta means slower.
    Compare {
        baseline: PathBuf,
        candidate: PathBuf,
        /// Minimum timing change to flag (percent); this is not a significance test.
        #[arg(long, default_value_t = 5.0)]
        threshold: f64,
        /// Reject conclusions when either relative interquartile range exceeds this.
        #[arg(long, default_value_t = 10.0)]
        noise_limit: f64,
        /// Exit 2 for a regression, 3 for inconclusive noisy data.
        #[arg(long)]
        fail_on_regression: bool,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    #[arg(
        long,
        default_value = ".work/arch/build/android-out/host/linux-x86/nativetest64/berberis_arm64_host_tests/berberis_arm64_host_tests"
    )]
    binary: PathBuf,
    /// Benchmark source used to build this binary; its hash guards workload changes.
    #[arg(
        long,
        default_value = ".work/translation/digitalis/lite_translator/arm64_to_x86_64/lite_translate_region_bench.cc"
    )]
    benchmark_source: PathBuf,
    /// Optional source inventory from the same build (recorded, not proof of binary provenance).
    #[arg(long)]
    build_manifest: Option<PathBuf>,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    label: String,
    /// Logical CPU for all test processes. Use the same idle CPU for A and B.
    #[arg(long)]
    cpu: u32,
    #[arg(long, value_enum, default_value = "all")]
    mode: Mode,
    /// Fresh correctness-suite processes in normal two-gear mode (tests exercise all tiers).
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..=100))]
    stress_repeats: u32,
    /// Fresh processes per case; each reports an upstream median of five timings.
    #[arg(long, default_value_t = 9, value_parser = clap::value_parser!(u32).range(5..=1000))]
    samples: u32,
    /// Timeout for each child process, including a correctness pass.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout_seconds: u64,
    /// Explicit comma-separated BERBERIS_FLAGS; inherited translator settings are removed.
    #[arg(long, default_value = "")]
    flags: String,
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    All,
    InterpretOnly,
    LiteTranslateOrInterpret,
    TwoGear,
}
impl Mode {
    fn names(self) -> Vec<&'static str> {
        match self {
            Self::All => vec!["interpret-only", "lite-translate-or-interpret", "two-gear"],
            Self::InterpretOnly => vec!["interpret-only"],
            Self::LiteTranslateOrInterpret => vec!["lite-translate-or-interpret"],
            Self::TwoGear => vec!["two-gear"],
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Conditions {
    machine: String,
    kernel: String,
    cpu: u32,
    cpu_model: String,
    cpu_flags: String,
    governor: String,
    boost: String,
    mode_names: Vec<String>,
    flags: String,
    benchmark_sha256: String,
    stress_repeats: u32,
}

#[derive(Serialize, Deserialize)]
struct Report {
    schema_version: u32,
    label: String,
    unix_seconds: u64,
    conditions: Conditions,
    binary: PathBuf,
    input_binary: PathBuf,
    binary_sha256: String,
    test_data: BTreeMap<String, String>,
    /// Hash adjacent Soong shared libraries as well as the statically linked translator.
    host_libraries: BTreeMap<String, String>,
    build_manifest: Option<serde_json::Value>,
    correctness: BTreeMap<String, Vec<serde_json::Value>>,
    cases: BTreeMap<String, Measurements>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Measurements {
    /// Upstream work-count label, NOT an exact retired-instruction hardware counter.
    reported_guest_insns: u64,
    inner_samples: u32,
    process_medians_ns: Vec<f64>,
}

fn hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hasher.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn inventory(root: &Path) -> Result<BTreeMap<String, String>> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                visit(root, &path, files)?;
            } else if kind.is_file() {
                files.insert(
                    path.strip_prefix(root)?
                        .to_str()
                        .ok_or("non-UTF8 test path")?
                        .into(),
                    hash(&path)?,
                );
            } else {
                return Err(format!("unsupported test data object: {}", path.display()).into());
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

fn snapshot_tree(source: &Path, destination: &Path) -> Result<BTreeMap<String, String>> {
    let before = inventory(source)?;
    for name in before.keys() {
        let target = destination.join(name);
        fs::create_dir_all(target.parent().ok_or("missing snapshot parent")?)?;
        fs::copy(source.join(name), target)?;
    }
    if inventory(source)? != before || inventory(destination)? != before {
        return Err(
            "host test build changed while snapshotting; wait for build completion and retry"
                .into(),
        );
    }
    Ok(before)
}

fn read_or_unknown(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "unknown".into())
}

fn cpu_field(block: &str, field: &str) -> Result<String> {
    block
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == field).then(|| value.trim().to_owned())
        })
        .ok_or_else(|| format!("CPU metadata missing {field}").into())
}

fn conditions(args: &RunArgs) -> Result<Conditions> {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo")?;
    let block = cpuinfo
        .split("\n\n")
        .find(|b| cpu_field(b, "processor").ok().as_deref() == Some(&args.cpu.to_string()))
        .ok_or("requested CPU is absent")?;
    Ok(Conditions {
        machine: fs::read_to_string("/etc/machine-id")?.trim().into(),
        kernel: fs::read_to_string("/proc/sys/kernel/osrelease")?
            .trim()
            .into(),
        cpu: args.cpu,
        cpu_model: cpu_field(block, "model name")?,
        cpu_flags: cpu_field(block, "flags")?,
        governor: read_or_unknown(format!(
            "/sys/devices/system/cpu/cpu{}/cpufreq/scaling_governor",
            args.cpu
        )),
        boost: format!(
            "boost={};intel_no_turbo={}",
            read_or_unknown("/sys/devices/system/cpu/cpufreq/boost"),
            read_or_unknown("/sys/devices/system/cpu/intel_pstate/no_turbo")
        ),
        mode_names: args.mode.names().into_iter().map(str::to_owned).collect(),
        flags: args.flags.clone(),
        benchmark_sha256: hash(&args.benchmark_source)?,
        stress_repeats: args.stress_repeats,
    })
}

/// Use file-backed stdout/stderr to avoid pipe deadlocks. Poll with a hard deadline.
fn execute(command: &mut Command, log: &Path, timeout: Duration) -> Result<String> {
    let file = File::create(log)?;
    command
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .process_group(0);
    let mut child = command.spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(
                    format!("test process failed ({status}); see {}", log.display()).into(),
                );
            }
            return Ok(fs::read_to_string(log)?);
        }
        if start.elapsed() >= timeout {
            // Death tests may fork; terminate our entire new group, not just its leader.
            let _ = Command::new("/usr/bin/kill")
                .args(["-KILL", "--", &format!("-{}", child.id())])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("test process timed out; see {}", log.display()).into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn command(args: &RunArgs, binary: &Path, mode: &str) -> Command {
    let mut cmd = Command::new("taskset");
    cmd.args(["--cpu-list", &args.cpu.to_string()]).arg(binary);
    // Sanitizing the environment prevents tracing, filters or LD_PRELOAD from
    // silently changing the workload. Soong's binary has an adjacent-library RPATH.
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("BERBERIS_MODE", mode)
        .arg("--gtest_color=no");
    if !args.flags.is_empty() {
        cmd.env("BERBERIS_FLAGS", &args.flags);
    }
    cmd
}

fn parse_bench(text: &str, expected: &str) -> Result<Measurements> {
    let rows: Vec<_> = text.lines().filter(|s| s.starts_with("BENCH ")).collect();
    if rows.len() != 1 {
        return Err("expected exactly one BENCH result".into());
    }
    let (name, rest) = rows[0].split_once(':').ok_or("missing BENCH colon")?;
    if name.trim() != format!("BENCH {expected}") {
        return Err("unexpected benchmark name".into());
    }
    let words: Vec<_> = rest.split_whitespace().collect();
    // <ns> ns / <count> insns = <Mips> Mips (median of <N>)
    if words.len() != 11
        || words[1..3] != ["ns", "/"]
        || words[4..6] != ["insns", "="]
        || words[7..10] != ["Mips", "(median", "of"]
    {
        return Err("unsupported BENCH output format".into());
    }
    let ns: f64 = words[0].parse()?;
    let count: u64 = words[3].parse()?;
    let inner: u32 = words[10]
        .strip_suffix(')')
        .ok_or("missing closing parenthesis")?
        .parse()?;
    if !ns.is_finite() || ns <= 0.0 || count == 0 || inner == 0 {
        return Err("invalid benchmark measurement".into());
    }
    Ok(Measurements {
        reported_guest_insns: count,
        inner_samples: inner,
        process_medians_ns: vec![ns],
    })
}

fn correctness_summary(path: &Path) -> Result<serde_json::Value> {
    let report: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    if report["tests"].as_u64().unwrap_or(0) == 0
        || report["failures"].as_u64() != Some(0)
        || report["errors"].as_u64().is_some_and(|n| n != 0)
    {
        return Err("correctness suite ran no tests or reported failure".into());
    }
    let tests: Vec<_> = report["testsuites"]
        .as_array()
        .ok_or("missing test suites")?
        .iter()
        .flat_map(|suite| suite["testsuite"].as_array().into_iter().flatten())
        .collect();
    if !tests
        .iter()
        .any(|test| test["status"] == "RUN" && test["result"] == "COMPLETED")
    {
        return Err("correctness suite completed no tests".into());
    }
    let skipped = tests
        .iter()
        .filter(|test| test["result"] == "SKIPPED")
        .count();
    Ok(serde_json::json!({"tests": report["tests"], "failures": 0,
        "disabled": report["disabled"], "skipped": skipped}))
}

fn run(mut args: RunArgs) -> Result<()> {
    if std::env::consts::OS != "linux" || std::env::consts::ARCH != "x86_64" {
        return Err(
            "requires an x86_64 Linux host; ARM64 phones do not exercise this translator".into(),
        );
    }
    let binary = args.binary.canonicalize()?;
    let mut elf = [0; 20];
    File::open(&binary)?.read_exact(&mut elf)?;
    if &elf[..6] != b"\x7fELF\x02\x01" || elf[18..20] != [62, 0] {
        return Err("expected the 64-bit x86_64 host test ELF".into());
    }
    let initial_conditions = conditions(&args)?;
    let binary_sha256 = hash(&binary)?;
    let libdir = binary
        .parent()
        .ok_or("missing binary parent")?
        .join("../../lib64");
    let host_libraries: BTreeMap<String, String> = ["libbase.so", "liblog.so", "libc++.so"]
        .into_iter()
        .map(|name| Ok((name.into(), hash(&libdir.join(name))?)))
        .collect::<Result<_>>()?;
    let build_manifest = args
        .build_manifest
        .as_ref()
        .map(|p| -> Result<_> { Ok(serde_json::from_slice(&fs::read(p)?)?) })
        .transpose()?;
    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(&args.output)?; // Never overwrite a baseline or logs.
    let output = args.output.canonicalize()?;
    let source_dir = binary.parent().ok_or("missing binary parent")?;
    if output.starts_with(source_dir) {
        return Err("output directory must be outside the host test data directory".into());
    }
    let snapshot_dir = output.join("runtime/nativetest64/berberis_arm64_host_tests");
    let mut test_data = snapshot_tree(source_dir, &snapshot_dir)?;
    let binary_name = binary.file_name().ok_or("missing binary filename")?;
    test_data.remove(binary_name.to_str().ok_or("non-UTF8 binary name")?);
    let snapshot_libdir = output.join("runtime/lib64");
    fs::create_dir_all(&snapshot_libdir)?;
    for (name, expected) in &host_libraries {
        fs::copy(libdir.join(name), snapshot_libdir.join(name))?;
        if hash(&libdir.join(name))? != *expected || hash(&snapshot_libdir.join(name))? != *expected
        {
            return Err("host library changed while snapshotting; retry after the build".into());
        }
    }
    let input_binary = binary.clone();
    let binary = snapshot_dir.join(binary_name);
    if hash(&binary)? != binary_sha256 {
        return Err("host test binary changed before snapshotting; retry after the build".into());
    }
    let source_snapshot = output.join("benchmark-source.cc");
    fs::copy(&args.benchmark_source, &source_snapshot)?;
    if hash(&source_snapshot)? != initial_conditions.benchmark_sha256 {
        return Err("benchmark source changed before snapshotting".into());
    }
    args.benchmark_source = source_snapshot;
    let timeout = Duration::from_secs(args.timeout_seconds);
    let mut report = Report {
        schema_version: 1,
        label: args.label.clone(),
        unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        conditions: initial_conditions,
        binary: binary.clone(),
        input_binary,
        binary_sha256,
        test_data,
        host_libraries,
        build_manifest,
        correctness: BTreeMap::new(),
        cases: BTreeMap::new(),
    };
    // Preserve inputs even when a gate later rejects the run. Only result.json
    // signifies a completed measurement; inputs.json must not be compared.
    fs::write(
        output.join("inputs.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    // Complete every correctness gate before accepting ANY performance results.
    // Global mode overrides are unsuitable for some upstream ABI/death tests.
    // Direct interpreter/lite/heavy tests already select their own entry points.
    for mode in ["two-gear"] {
        for repeat in 0..args.stress_repeats {
            eprintln!(
                "Correctness {mode}: pass {}/{}",
                repeat + 1,
                args.stress_repeats
            );
            let stem = format!("correctness-{mode}-{repeat}");
            let json = output.join(format!("{stem}.json"));
            let mut cmd = command(&args, &binary, mode);
            cmd.arg("--gtest_filter=-DigitalisBench.*")
                .arg(format!("--gtest_output=json:{}", json.display()));
            execute(&mut cmd, &output.join(format!("{stem}.log")), timeout)?;
            let summary = correctness_summary(&json)?;
            eprintln!("  {summary}");
            report
                .correctness
                .entry(mode.into())
                .or_default()
                .push(summary);
        }
    }
    // Rotate case order between sweeps, keeping all cases isolated in fresh processes.
    for sample in 0..args.samples {
        eprintln!("Benchmark sweep {}/{}", sample + 1, args.samples);
        for mode in args.mode.names() {
            for index in 0..CASES.len() {
                let (test, name) = CASES[(index + sample as usize) % CASES.len()];
                let mut cmd = command(&args, &binary, mode);
                cmd.arg(format!("--gtest_filter=DigitalisBench.{test}"));
                let log = output.join(format!("bench-{mode}-{name}-{sample}.log"));
                let text = execute(&mut cmd, &log, timeout)?;
                let measured = parse_bench(&text, name)?;
                let row = report
                    .cases
                    .entry(format!("{mode}/{name}"))
                    .or_insert_with(|| Measurements {
                        reported_guest_insns: measured.reported_guest_insns,
                        inner_samples: measured.inner_samples,
                        process_medians_ns: Vec::new(),
                    });
                if row.reported_guest_insns != measured.reported_guest_insns
                    || row.inner_samples != measured.inner_samples
                {
                    return Err("benchmark workload changed during run".into());
                }
                row.process_medians_ns.extend(measured.process_medians_ns);
            }
        }
    }
    if conditions(&args)? != report.conditions || hash(&binary)? != report.binary_sha256 {
        return Err("host settings, benchmark source or binary changed during run".into());
    }
    for (name, before) in &report.host_libraries {
        if hash(&snapshot_libdir.join(name))? != *before {
            return Err("host library changed during run".into());
        }
    }
    let mut final_data = inventory(&snapshot_dir)?;
    final_data.remove(binary_name.to_str().ok_or("non-UTF8 binary name")?);
    if final_data != report.test_data {
        return Err("test data changed during run".into());
    }
    fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{:<38} {:>12} {:>10}", "Mode/case", "Median ms", "IQR %");
    for (name, row) in &report.cases {
        let (median, iqr) = stats(&row.process_medians_ns)?;
        println!("{name:<38} {:>12.3} {:>10.2}", median / 1e6, iqr);
    }
    println!("Saved {}", output.join("result.json").display());
    Ok(())
}

fn stats(samples: &[f64]) -> Result<(f64, f64)> {
    if samples.len() < 5 || samples.iter().any(|n| !n.is_finite() || *n <= 0.0) {
        return Err("need at least five finite, positive samples".into());
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let quantile = |fraction: f64| {
        let pos = (sorted.len() - 1) as f64 * fraction;
        let lo = pos.floor() as usize;
        let hi = pos.ceil() as usize;
        sorted[lo] + (sorted[hi] - sorted[lo]) * pos.fract()
    };
    let median = quantile(0.5);
    Ok((median, 100.0 * (quantile(0.75) - quantile(0.25)) / median))
}

fn compatible(a: &Report, b: &Report) -> Result<()> {
    if a.schema_version != 1 || b.schema_version != 1 || a.conditions != b.conditions {
        return Err(
            "incompatible run schema, machine/CPU, mode, settings or benchmark source".into(),
        );
    }
    if a.correctness != b.correctness || a.correctness.is_empty() {
        return Err("correctness counts/skips changed; review coverage before comparing".into());
    }
    if a.host_libraries != b.host_libraries || a.test_data != b.test_data {
        return Err(
            "host support libraries or test data changed; use matching inputs for comparison"
                .into(),
        );
    }
    if a.cases.is_empty() || !a.cases.keys().eq(b.cases.keys()) {
        return Err("benchmark case sets differ or are empty".into());
    }
    for (name, old) in &a.cases {
        let new = &b.cases[name];
        if old.reported_guest_insns != new.reported_guest_insns
            || old.inner_samples != new.inner_samples
        {
            return Err(format!("workload changed for {name}").into());
        }
    }
    Ok(())
}

fn verdict(delta: f64, old_iqr: f64, new_iqr: f64, threshold: f64, noise: f64) -> &'static str {
    if old_iqr > noise || new_iqr > noise {
        "NOISY"
    } else if delta > threshold {
        "SLOWER"
    } else if delta < -threshold {
        "FASTER"
    } else {
        "within threshold"
    }
}

fn compare(a: &Path, b: &Path, threshold: f64, noise: f64, gate: bool) -> Result<i32> {
    if !threshold.is_finite() || threshold <= 0.0 || !noise.is_finite() || noise <= 0.0 {
        return Err("threshold and noise limit must be finite and positive".into());
    }
    let a: Report = serde_json::from_slice(&fs::read(a)?)?;
    let b: Report = serde_json::from_slice(&fs::read(b)?)?;
    compatible(&a, &b)?;
    println!("{} -> {} (positive time delta = slower)", a.label, b.label);
    if a.binary_sha256 == b.binary_sha256 {
        println!("Same test binary: no translator binary change between these reports.");
    }
    println!(
        "{:<38} {:>10} {:>10} {:>9} {:>13}  Verdict",
        "Mode/case", "Before ms", "After ms", "Delta %", "IQR % A/B"
    );
    let mut slower = false;
    let mut noisy = false;
    for (name, old) in &a.cases {
        let (old_ns, old_iqr) = stats(&old.process_medians_ns)?;
        let (new_ns, new_iqr) = stats(&b.cases[name].process_medians_ns)?;
        let delta = 100.0 * (new_ns / old_ns - 1.0);
        let status = verdict(delta, old_iqr, new_iqr, threshold, noise);
        slower |= status == "SLOWER";
        noisy |= status == "NOISY";
        println!(
            "{name:<38} {:>10.3} {:>10.3} {delta:>+9.2} {:>6.2}/{:<6.2} {status}",
            old_ns / 1e6,
            new_ns / 1e6,
            old_iqr,
            new_iqr
        );
    }
    println!(
        "Screening signal only: repeat A/B or ABBA runs before accepting a performance change."
    );
    Ok(if gate && slower {
        2
    } else if gate && noisy {
        3
    } else {
        0
    })
}

fn main() {
    let result = match Cli::parse().action {
        Action::Run(args) => run(args).map(|()| 0),
        Action::Compare {
            baseline,
            candidate,
            threshold,
            noise_limit,
            fail_on_regression,
        } => compare(
            &baseline,
            &candidate,
            threshold,
            noise_limit,
            fail_on_regression,
        ),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_output_and_rejects_missing_duplicate_wrong_and_invalid_results() {
        let line =
            "BENCH int     :    4587155 ns /  54000000 insns =  11772.0 Mips (median of 5)\n";
        let row = parse_bench(line, "int").unwrap();
        assert_eq!(row.process_medians_ns, [4587155.0]);
        assert_eq!(row.reported_guest_insns, 54_000_000);
        assert_eq!(row.inner_samples, 5);
        for text in [
            String::new(),
            line.repeat(2),
            line.replace("4587155", "NaN"),
            line.replace("4587155", "0"),
            line.replace("54000000", "0"),
            line.replace("ns /", "us /"),
            line.replace("BENCH int", "BENCH fp"),
        ] {
            assert!(parse_bench(&text, "int").is_err(), "{text}");
        }
    }

    #[test]
    fn statistics_and_noise_do_not_confuse_improvement_with_regression() {
        assert_eq!(stats(&[5., 1., 4., 2., 3.]).unwrap(), (3., 200. / 3.));
        assert!(stats(&[1.; 4]).is_err());
        assert!(stats(&[f64::INFINITY; 5]).is_err());
        assert_eq!(verdict(-15., 1., 2., 5., 10.), "FASTER");
        assert_eq!(verdict(15., 1., 2., 5., 10.), "SLOWER");
        assert_eq!(verdict(15., 11., 2., 5., 10.), "NOISY");
        assert_eq!(verdict(-15., 1., 12., 5., 10.), "NOISY");
        assert_eq!(verdict(2., 1., 2., 5., 10.), "within threshold");
    }

    fn fixture() -> Report {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1, "label": "test", "unix_seconds": 1,
            "conditions": {
                "machine": "host", "kernel": "kernel", "cpu": 6,
                "cpu_model": "cpu", "cpu_flags": "flags", "governor": "performance",
                "boost": "1", "mode_names": ["two-gear"], "flags": "",
                "benchmark_sha256": "source", "stress_repeats": 2
            },
            "binary": "/test", "input_binary": "/original", "binary_sha256": "binary", "test_data": {},
            "host_libraries": {"libbase.so": "library"}, "build_manifest": null,
            "correctness": {"two-gear": [{"tests": 100, "failures": 0, "skipped": 3}]},
            "cases": {"two-gear/int": {
                "reported_guest_insns": 54000000, "inner_samples": 5,
                "process_medians_ns": [100.0, 100.0, 100.0, 100.0, 100.0]
            }}
        })).unwrap()
    }

    #[test]
    fn comparison_accepts_changed_translator_but_rejects_changed_conditions_or_work() {
        let old = fixture();
        let mut new = fixture();
        new.binary_sha256 = "new-translator".into();
        compatible(&old, &new).unwrap();
        new.conditions.cpu = 7;
        assert!(compatible(&old, &new).is_err());
        new = fixture();
        new.conditions.benchmark_sha256 = "new-loop".into();
        assert!(compatible(&old, &new).is_err());
        new = fixture();
        new.cases
            .get_mut("two-gear/int")
            .unwrap()
            .reported_guest_insns += 1;
        assert!(compatible(&old, &new).is_err());
        new = fixture();
        new.correctness.clear();
        assert!(compatible(&old, &new).is_err());
        new = fixture();
        new.host_libraries.clear();
        assert!(compatible(&old, &new).is_err());
    }

    #[test]
    fn regression_gate_and_noisy_gate_have_distinct_exit_codes() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("a.json");
        let new = dir.path().join("b.json");
        fs::write(&old, serde_json::to_vec(&fixture()).unwrap()).unwrap();
        let mut candidate = fixture();
        candidate
            .cases
            .get_mut("two-gear/int")
            .unwrap()
            .process_medians_ns = vec![120.; 5];
        fs::write(&new, serde_json::to_vec(&candidate).unwrap()).unwrap();
        assert_eq!(compare(&old, &new, 5., 10., true).unwrap(), 2);
        assert_eq!(compare(&old, &new, 5., 10., false).unwrap(), 0);
        candidate
            .cases
            .get_mut("two-gear/int")
            .unwrap()
            .process_medians_ns = vec![80., 90., 100., 120., 160.];
        fs::write(&new, serde_json::to_vec(&candidate).unwrap()).unwrap();
        assert_eq!(compare(&old, &new, 5., 10., true).unwrap(), 3);
    }

    #[test]
    fn correctness_gate_rejects_failure_or_empty_suite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("correctness.json");
        for value in [
            serde_json::json!({"tests": 0, "failures": 0}),
            serde_json::json!({"tests": 100, "failures": 1}),
        ] {
            fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(correctness_summary(&path).is_err());
        }
    }

    #[test]
    fn timeout_and_failure_preserve_diagnostic_logs() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("child.log");
        assert!(
            execute(
                Command::new("sleep").arg("5"),
                &log,
                Duration::from_millis(30)
            )
            .unwrap_err()
            .to_string()
            .contains("timed out")
        );
        assert!(log.exists());
        assert!(execute(&mut Command::new("false"), &log, Duration::from_secs(2)).is_err());
    }

    #[test]
    fn snapshot_is_independent_of_later_builds_and_includes_nested_test_data() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let snapshot = dir.path().join("snapshot");
        fs::create_dir_all(source.join("decoder/arm64_test")).unwrap();
        fs::write(source.join("test-binary"), b"old executable").unwrap();
        fs::write(source.join("decoder/arm64_test/corpus"), b"test inputs").unwrap();
        let before = snapshot_tree(&source, &snapshot).unwrap();
        fs::write(source.join("test-binary"), b"rebuilt executable").unwrap();
        assert_eq!(inventory(&snapshot).unwrap(), before);
        assert_ne!(inventory(&source).unwrap(), before);
        assert_eq!(
            fs::read(snapshot.join("decoder/arm64_test/corpus")).unwrap(),
            b"test inputs"
        );
    }
}
