// Exercise production shutdown methods with real libcamera-base queues and
// simulated capture/IPA events. No camera, DMA allocator or image data is used.
use std::{env, error::Error, fs, path::Path, process::Command};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn function(source: &str, name: &str) -> Result<String> {
    let start = source.find(name).ok_or_else(|| format!("missing {name}"))?;
    let end = source[start..].find("\n}").ok_or("unterminated function")?;
    Ok(source[start..start + end + 2].into())
}

fn compile(source: &Path, build: &Path, output: &Path) -> Result<()> {
    fs::create_dir(output)?;
    let pipeline = fs::read_to_string(source.join("src/libcamera/pipeline/simple/simple.cpp"))?;
    let isp = fs::read_to_string(source.join("src/libcamera/software_isp/software_isp.cpp"))?;
    let probe = include_str!("worker-regression.cc.in")
        .replace("@ISP_STOP@", &function(&isp, "void SoftwareIsp::stop(")?)
        .replace(
            "@PIPELINE_STOP@",
            &function(&pipeline, "void SimplePipelineHandler::stopDevice(")?,
        );
    let cpp = output.join("worker-regression.cpp");
    fs::write(&cpp, probe)?;
    let compiler = env::var_os("CXX").unwrap_or_else(|| "c++".into());
    let status = Command::new(compiler)
        .args([
            "-std=c++20",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-DLIBCAMERA_BASE_PRIVATE",
        ])
        .arg("-I")
        .arg(source.join("include"))
        .arg("-I")
        .arg(build.join("include"))
        .arg("-include")
        .arg(build.join("config.h"))
        .arg(&cpp)
        .arg("-L")
        .arg(build.join("src/libcamera/base"))
        .args(["-lcamera-base", "-pthread", "-o"])
        .arg(output.join("worker-regression"))
        .status()?;
    if !status.success() {
        return Err("worker regression compilation failed".into());
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() != 4 {
        return Err("usage: worker-regression BASELINE_SOURCE CANDIDATE_SOURCE BUILD_DIRECTORY NEW_OUTPUT_DIRECTORY".into());
    }
    let baseline = Path::new(&args[0]);
    let candidate = Path::new(&args[1]);
    let build = fs::canonicalize(&args[2])?;
    let output = Path::new(&args[3]);
    fs::create_dir(output)?;
    let output = fs::canonicalize(output)?;
    compile(baseline, &build, &output.join("baseline"))?;
    compile(candidate, &build, &output.join("candidate"))?;
    let mut red_log = String::new();
    let mut green_log = String::new();
    for iteration in 1..=5 {
        for (name, expected, log) in [
            ("baseline", 1, &mut red_log),
            ("candidate", 0, &mut green_log),
        ] {
            let result = Command::new(output.join(name).join("worker-regression"))
                .env("LD_LIBRARY_PATH", build.join("src/libcamera/base"))
                .output()?;
            let text = String::from_utf8(result.stdout)? + &String::from_utf8(result.stderr)?;
            log.push_str(&format!("iteration={iteration}\n{text}"));
            let tokens = if expected == 1 {
                [
                    "late_captures=1",
                    "expired_worker_frames=1",
                    "process_before_start=1",
                    "expired_owner_callbacks=2",
                    "untracked_owner_callbacks=2",
                ]
            } else {
                [
                    "late_captures=0",
                    "expired_worker_frames=0",
                    "process_before_start=0",
                    "expired_owner_callbacks=0",
                    "untracked_owner_callbacks=0",
                ]
            };
            if result.status.code() != Some(expected)
                || !text.contains("callbacks_drained_before_free=2")
                || !tokens.iter().all(|token| text.contains(token))
            {
                return Err(format!(
                    "{name} iteration {iteration} violated the queue contract: {text}"
                )
                .into());
            }
        }
    }
    fs::write(output.join("baseline.log"), red_log)?;
    fs::write(output.join("candidate.log"), green_log)?;
    println!("Worker restart: five red/green pairs passed using production shutdown methods");
    println!(
        "Worker replay and late owner callbacks reproduced before producer quiescence; both absent after it"
    );
    Ok(())
}
