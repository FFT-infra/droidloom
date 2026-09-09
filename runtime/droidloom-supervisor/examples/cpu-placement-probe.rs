//! Non-UI integration check. Run as root in a disposable mount namespace:
//! unshare --mount --propagation private ./cpu-placement-probe /run/dl-cpu-probe
#[path = "../src/cpu_placement.rs"]
mod cpu_placement;
use droidloom_cpu_placement::{CpuSet, Groups};
use std::{fs, path::Path, process::Command};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--child") {
        assert_eq!(CpuSet::current()?.list(), args[2]);
        return Ok(());
    }
    assert_ne!(
        fs::read_link("/proc/self/ns/mnt")?,
        fs::read_link("/proc/1/ns/mnt")?,
        "run in a disposable mount namespace"
    );
    let root = Path::new(args.get(1).ok_or("missing disposable directory")?);
    fs::create_dir(root)?;
    let groups = Groups::detect()?.ok_or("probe requires a heterogeneous CPU domain")?;
    let before = fs::read_to_string("/proc/self/cgroup")?;
    let v2_path = before
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("missing v2 membership")?;
    let v2_procs = format!("/sys/fs/cgroup{v2_path}/cgroup.procs");
    let pid = std::process::id().to_string();
    assert!(
        fs::read_to_string(&v2_procs)?
            .lines()
            .any(|line| line == pid)
    );
    assert!(cpu_placement::prepare(root, "integration-probe")?.is_some());

    // Exercise Android's exact remount after the new cgroup namespace exists.
    // Android must not detect pre-mounted declared paths and skip CgroupSetup.
    assert!(!root.join("dev/cpuset").exists());
    assert!(!root.join("dev/blkio").exists());
    assert!(
        !root
            .join("dev/.droidloom-cgroups/cpuset-discovery")
            .exists()
    );
    assert!(!root.join("dev/.droidloom-cgroups/blkio-discovery").exists());
    let remount = root.join("dev/cpuset");
    fs::create_dir(&remount)?;
    assert!(
        Command::new("mount")
            .args([
                "-t",
                "cgroup",
                "-o",
                "cpuset,noprefix,cpuset_v2_mode",
                "none"
            ])
            .arg(&remount)
            .status()?
            .success()
    );
    assert!(
        !remount.join("droidloom-integration-probe").exists(),
        "ancestor cpuset leaked into cell"
    );
    assert_eq!(
        CpuSet::parse(&fs::read_to_string(remount.join("cpus"))?)?,
        groups.normal
    );
    let io_root = root.join("dev/blkio");
    fs::create_dir(&io_root)?;
    assert!(
        Command::new("mount")
            .args(["-t", "cgroup", "-o", "blkio", "none"])
            .arg(&io_root)
            .status()?
            .success()
    );
    assert!(!io_root.join("droidloom-integration-probe").exists());
    fs::create_dir(io_root.join("background"))?;
    fs::write(io_root.join("background/tasks"), "0")?;
    fs::write(io_root.join("tasks"), "0")?;
    fs::remove_dir(io_root.join("background"))?;
    let mems = fs::read_to_string(remount.join("mems"))?;
    for (group, mask) in [
        ("top-app", &groups.graphics),
        ("foreground", &groups.normal),
        ("background", &groups.background),
    ] {
        let path = remount.join(group);
        fs::create_dir(&path)?;
        fs::write(path.join("mems"), &mems)?;
        fs::write(path.join("cpus"), mask.list())?;
    }
    fs::write(remount.join("top-app/cgroup.procs"), "0")?;
    assert_eq!(CpuSet::current()?, groups.graphics);
    let child_root = remount.clone();
    let child_groups = groups.clone();
    std::thread::spawn(move || {
        assert_eq!(CpuSet::current().unwrap(), child_groups.graphics);
        fs::write(child_root.join("background/tasks"), "0").unwrap();
        assert_eq!(CpuSet::current().unwrap(), child_groups.background);
        let expected = child_groups.background.clone();
        std::thread::spawn(move || assert_eq!(CpuSet::current().unwrap(), expected))
            .join()
            .unwrap();
    })
    .join()
    .unwrap();
    assert_eq!(
        CpuSet::current()?,
        groups.graphics,
        "background override changed creator"
    );
    assert!(
        Command::new(std::env::current_exe()?)
            .args(["--child", &groups.graphics.list()])
            .status()?
            .success()
    );
    fs::write(remount.join("foreground/cgroup.procs"), "0")?;
    assert_eq!(
        CpuSet::current()?,
        groups.normal,
        "foreground transition failed to restore normal domain"
    );
    assert!(
        fs::read_to_string(&v2_procs)?
            .lines()
            .any(|line| line == pid),
        "CPU placement moved the process out of its v2 domain"
    );
    fs::write(remount.join("cgroup.procs"), "0")?;
    for group in ["top-app", "foreground", "background"] {
        fs::remove_dir(remount.join(group))?;
    }
    println!(
        "PASS: private remount, big/little/normal transitions, thread and exec inheritance, unchanged v2 membership"
    );
    Ok(())
}
