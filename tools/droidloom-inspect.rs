//! Optional passwordless, read-only diagnostics for the local u1000 cell.
//! Build with rustc; install the binary root-owned before granting sudo access.
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::process::{Command, ExitCode};

fn arguments(mode: &str) -> Result<&'static [&'static str], String> {
    match mode {
        "features" => Ok(&["/system/bin/cmd", "package", "list", "features"]),
        "properties" => Ok(&["/system/bin/getprop"]),
        "configuration" => Ok(&["/system/bin/cmd", "activity", "get-config"]),
        _ => Err("usage: droidloom-inspect features|properties|configuration".into()),
    }
}

fn same_object(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 1 {
        return Err("exactly one diagnostic mode is required".into());
    }
    let android_args = arguments(&args[0])?;
    // This installation is deliberately limited to the development user's cell.
    // Neither a PID, namespace path, executable nor extra arguments are accepted.
    if std::env::var("SUDO_UID").as_deref() != Ok("1000") {
        return Err("this helper is restricted to sudo caller UID 1000".into());
    }
    let network = File::open("/run/netns/droidloom-u1000")?;
    let listing = Command::new("/usr/bin/ip")
        .env_clear()
        .args(["netns", "pids", "droidloom-u1000"])
        .output()?;
    if !listing.status.success() {
        return Err("cannot enumerate the Droidloom network namespace".into());
    }
    for candidate in std::str::from_utf8(&listing.stdout)?.split_whitespace() {
        let pid: u32 = candidate.parse()?;
        // Pin the proc directory before opening any process resources. If init
        // exits, these lookups fail rather than following a recycled host PID.
        let Ok(proc_dir) = File::open(format!("/proc/{pid}")) else {
            continue;
        };
        let proc_path = format!("/proc/{}/fd/{}", std::process::id(), proc_dir.as_raw_fd());
        let Ok(status) = fs::read_to_string(format!("{proc_path}/status")) else {
            continue;
        };
        let ids: Vec<_> = status
            .lines()
            .find_map(|l| l.strip_prefix("NSpid:"))
            .unwrap_or("")
            .split_whitespace()
            .collect();
        if ids.len() < 2 || ids.last() != Some(&"1") {
            continue;
        }
        let net = File::open(format!("{proc_path}/ns/net"))?;
        if !same_object(&network.metadata()?, &net.metadata()?) {
            continue;
        }
        let mut held = vec![net];
        let mut command = Command::new("/usr/bin/timeout");
        command
            .env_clear()
            .env("PATH", "/system/bin:/system/xbin")
            .env("ANDROID_ROOT", "/system")
            .env("ANDROID_DATA", "/data")
            .args(["--kill-after=1s", "15s", "/usr/bin/nsenter"]);
        command.arg(format!(
            "--net=/proc/{}/fd/{}",
            std::process::id(),
            held[0].as_raw_fd()
        ));
        for (flag, resource) in [
            ("mount", "ns/mnt"),
            ("uts", "ns/uts"),
            ("ipc", "ns/ipc"),
            ("pid", "ns/pid"),
            ("cgroup", "ns/cgroup"),
            ("root", "root"),
        ] {
            let file = File::open(format!("{proc_path}/{resource}"))?;
            command.arg(format!(
                "--{flag}=/proc/{}/fd/{}",
                std::process::id(),
                file.as_raw_fd()
            ));
            held.push(file);
        }
        // nsenter opens all namespace/root references before changing namespaces.
        // The parent retains the descriptors until the bounded child exits.
        let result = command.args(["--wd=/", "--"]).args(android_args).status()?;
        if !result.success() {
            return Err(format!("diagnostic failed: {result}").into());
        }
        return Ok(());
    }
    Err("no live Android init in the u1000 Droidloom namespace".into())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("droidloom-inspect: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_commands_paths_and_extra_arguments() {
        for input in [
            "",
            "shell",
            "features; sh",
            "/bin/sh",
            "features --target 1",
            "--help",
        ] {
            assert!(arguments(input).is_err());
        }
    }
    #[test]
    fn exposes_only_fixed_read_only_commands() {
        assert_eq!(
            arguments("features").unwrap(),
            ["/system/bin/cmd", "package", "list", "features"]
        );
        assert_eq!(arguments("properties").unwrap(), ["/system/bin/getprop"]);
        assert_eq!(
            arguments("configuration").unwrap(),
            ["/system/bin/cmd", "activity", "get-config"]
        );
    }
}
