//! Cell-private legacy cpuset backend for Android's native task-profile API.
//!
//! The controller must exist before CLONE_NEWCGROUP. Android otherwise cannot
//! create a new legacy hierarchy from its noninitial cgroup namespace (EPERM).
//! The separate hierarchy leaves Android's v2 memory/freezer groups untouched.
use droidloom_cpu_placement::{CpuSet, Groups};
use std::{ffi::CString, fs, io, path::Path};

fn mount(
    source: &str,
    target: &Path,
    kind: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(source)?;
    let target = CString::new(target.as_os_str().as_bytes())?;
    let kind = kind.map(CString::new).transpose()?;
    let data = data.map(CString::new).transpose()?;
    // SAFETY: all strings live through mount; it retains no userspace pointers.
    let result = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            kind.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |s| s.as_ptr().cast()),
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Create the hierarchy in the initial cgroup namespace, enter only our subtree,
/// then seal the namespace. A missing/busy legacy controller is explicitly
/// reported; Android may still boot using its original optional descriptors.
pub(crate) fn prepare(root: &Path, cell: &str) -> io::Result<Option<String>> {
    let normal = CpuSet::current()?;
    let groups = match Groups::detect() {
        Ok(groups) => groups,
        Err(error) => {
            eprintln!(
                "Droidloom Android CPU topology unavailable: {error}; using the inherited domain for every profile"
            );
            None
        }
    };
    // Android's scheduling aggregates also contain I/O actions. Without a real
    // blkio hierarchy they report failure even after applying CPU placement.
    prepare_legacy(root, "blkio", "blkio", cell, |_| Ok(()))?;
    let cpuset = prepare_legacy(
        root,
        "cpuset",
        "cpuset,noprefix,cpuset_v2_mode",
        cell,
        |child| {
            let status = fs::read_to_string("/proc/self/status")?;
            let mems = status
                .lines()
                .find_map(|line| line.strip_prefix("Mems_allowed_list:"))
                .ok_or_else(|| io::Error::other("missing inherited memory-node domain"))?
                .trim();
            fs::write(child.join("mems"), mems)?;
            fs::write(child.join("cpus"), normal.list())
        },
    )?;
    // SAFETY: single-threaded cell entry, before Android starts. Each legacy
    // root is now private; v2 still captures the original lifecycle domain.
    if unsafe { libc::unshare(libc::CLONE_NEWCGROUP) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if cpuset {
        eprintln!(
            "Droidloom Android cpuset: cell={cell} normal={} big={} little={}",
            normal.list(),
            groups
                .as_ref()
                .map_or_else(|| normal.list(), |g| g.graphics.list()),
            groups
                .as_ref()
                .map_or_else(|| normal.list(), |g| g.background.list())
        );
        Ok(Some(init_policy(&normal, groups.as_ref())))
    } else {
        Ok(None)
    }
}

fn prepare_legacy(
    root: &Path,
    controller: &str,
    options: &str,
    cell: &str,
    configure: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<bool> {
    // Android's CgroupSetup returns early when ANY declared v1 mount already
    // exists. Retain the prepared subtree at a private holding path instead;
    // Android must mount /dev/{controller} itself and finish v2 initialization.
    let holding = root.join("dev/.droidloom-cgroups");
    fs::create_dir_all(&holding)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&holding, fs::Permissions::from_mode(0o700))?;
    let target = holding.join(format!("{controller}-discovery"));
    fs::create_dir_all(&target)?;
    if let Err(error) = mount(
        "none",
        &target,
        Some("cgroup"),
        libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        Some(options),
    ) {
        eprintln!(
            "Droidloom Android {controller} backend unavailable: {error}. The host may own it in cgroup v2 or disable legacy controllers; a different platform backend is required. Profiles using this controller remain unavailable."
        );
        return Ok(false);
    }
    // Stay within the host's existing domain. Never configure or join the
    // hierarchy root, and never remove a group which still has live tasks.
    let membership = fs::read_to_string("/proc/self/cgroup")?;
    let parent = membership
        .lines()
        .find_map(|line| {
            let mut fields = line.splitn(3, ':');
            fields.next()?;
            let controllers = fields.next()?;
            let path = fields.next()?;
            controllers
                .split(',')
                .any(|name| name == controller)
                .then_some(path)
        })
        .ok_or_else(|| io::Error::other("mounted controller has no process membership"))?;
    if parent.split('/').any(|part| part == "..") {
        return Err(io::Error::other(
            "controller ancestor is outside this namespace",
        ));
    }
    let child = target
        .join(parent.trim_start_matches('/'))
        .join(format!("droidloom-{cell}"));
    if child.exists() {
        remove_empty_group(&child)?;
    }
    fs::create_dir(&child)?;
    configure(&child)?;
    fs::write(child.join("cgroup.procs"), "0")?;
    let private = holding.join(controller);
    fs::create_dir_all(&private)?;
    mount(
        child
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF8 cgroup path"))?,
        &private,
        None,
        libc::MS_BIND,
        None,
    )?;
    // Retain only the cell subtree. No buried full-hierarchy mount may pass
    // into Android, even behind a private directory or another bind mount.
    use std::os::unix::ffi::OsStrExt;
    let discovery = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: the path remains alive and this namespace owns the mount.
    if unsafe { libc::umount2(discovery.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    fs::remove_dir(&target)?;
    eprintln!("Droidloom Android {controller}: private cell subtree ready");
    Ok(true)
}

fn init_policy(normal: &CpuSet, groups: Option<&Groups>) -> String {
    let big = groups.map_or(normal, |g| &g.graphics).list();
    let little = groups.map_or(normal, |g| &g.background).list();
    let normal = normal.list();
    // Appended after the platform's on-init defaults, before Android services
    // start. Group names retain get_sched_policy()'s native interpretation.
    let mut rc = String::from(
        "\n# Droidloom CPU placement: generated from the host's allowed topology.\non init\n",
    );
    for (group, cpus) in [
        ("foreground", &normal),
        ("foreground_window", &normal),
        ("system", &normal),
        ("camera-daemon", &normal),
        ("nnapi-hal", &normal),
        ("top-app", &big),
        ("rt", &big),
        ("background", &little),
        ("system-background", &little),
        ("restricted", &little),
        ("dex2oat", &little),
    ] {
        rc.push_str(&format!("    mkdir /dev/cpuset/{group} 0755 system system\n    copy /dev/cpuset/mems /dev/cpuset/{group}/mems\n    write /dev/cpuset/{group}/cpus {cpus}\n    chown system system /dev/cpuset/{group}/tasks\n    chown system system /dev/cpuset/{group}/cgroup.procs\n    chmod 0664 /dev/cpuset/{group}/tasks\n    chmod 0664 /dev/cpuset/{group}/cgroup.procs\n"));
    }
    rc
}

/// Translate CPU placement profiles onto the actual cell controller. Other
/// actions (I/O, memory, freezer, scheduler priority, timer slack) are preserved.
/// Frequency/utilization-clamp attributes deliberately retain their own backend.
pub(crate) fn task_profiles(source: &str) -> io::Result<String> {
    let mut json: serde_json::Value = serde_json::from_str(source)?;
    let Some(profiles) = json
        .get_mut("Profiles")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(source.into());
    };
    for profile in profiles {
        let graphics = matches!(
            profile["Name"].as_str(),
            Some("SFMainPolicy" | "SFRenderEnginePolicy")
        );
        if let Some(actions) = profile["Actions"].as_array_mut() {
            for action in actions {
                if action["Name"] == "JoinCgroup" && action["Params"]["Controller"] == "cpu" {
                    let path = match action["Params"]["Path"].as_str() {
                        Some("" | "system" | "camera-daemon" | "nnapi-hal") => "foreground",
                        Some("rt" | "top-app") => "top-app",
                        Some("dex2oat" | "background") => "background",
                        Some("foreground") => "foreground",
                        Some("foreground_window") => "foreground_window",
                        Some("system-background") => "system-background",
                        _ => {
                            return Err(io::Error::other(
                                "Android CPU profile needs an explicit platform group mapping",
                            ));
                        }
                    };
                    action["Params"]["Controller"] = "cpuset".into();
                    action["Params"]["Path"] = path.into();
                }
                if graphics
                    && action["Name"] == "JoinCgroup"
                    && action["Params"]["Controller"] == "cpuset"
                {
                    action["Params"]["Path"] = "top-app".into();
                }
            }
        }
    }
    serde_json::to_string_pretty(&json).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boot_policy_respects_capacity_classes_and_homogeneous_fallback() {
        let normal = CpuSet::parse("0-7").unwrap();
        let groups = Groups::new(
            normal.clone(),
            CpuSet::parse("3-7").unwrap(),
            CpuSet::parse("0-2").unwrap(),
        )
        .unwrap();
        let rc = init_policy(&normal, Some(&groups));
        assert!(rc.contains("write /dev/cpuset/top-app/cpus 3,4,5,6,7"));
        assert!(rc.contains("write /dev/cpuset/background/cpus 0,1,2"));
        assert!(rc.contains("write /dev/cpuset/foreground/cpus 0,1,2,3,4,5,6,7"));
        assert!(!rc.contains("uclamp"));
        assert!(
            init_policy(&normal, None)
                .contains("write /dev/cpuset/background/cpus 0,1,2,3,4,5,6,7")
        );
    }
    #[test]
    fn translates_placement_without_dropping_unrelated_actions() {
        let source = r#"{"Profiles":[
          {"Name":"HighPerformance","Actions":[{"Name":"JoinCgroup","Params":{"Controller":"cpu","Path":"foreground"}}]},
          {"Name":"SFMainPolicy","Actions":[{"Name":"JoinCgroup","Params":{"Controller":"cpuset","Path":"system-background"}}]},
          {"Name":"Frozen","Actions":[{"Name":"SetAttribute","Params":{"Name":"FreezerState","Value":"1"}}]}],
          "AggregateProfiles":[{"Name":"Custom","Profiles":["HighPerformance","Frozen"]}]}"#;
        let before: serde_json::Value = serde_json::from_str(source).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&task_profiles(source).unwrap()).unwrap();
        assert_eq!(
            after["Profiles"][0]["Actions"][0]["Params"]["Controller"],
            "cpuset"
        );
        assert_eq!(
            after["Profiles"][1]["Actions"][0]["Params"]["Path"],
            "top-app"
        );
        assert_eq!(after["Profiles"][2], before["Profiles"][2]);
        assert_eq!(after["AggregateProfiles"], before["AggregateProfiles"]);
    }
}

// A hierarchy can outlive its last mount. Reclaim only our empty prior cell;
// remove_dir is the kernel's final race-safe check that a group has no tasks.
fn remove_empty_group(path: &Path) -> io::Result<()> {
    if !fs::read_to_string(path.join("cgroup.procs"))?
        .trim()
        .is_empty()
    {
        return Err(io::Error::other(
            "previous Droidloom cpuset still contains processes",
        ));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_empty_group(&entry.path())?;
        }
    }
    fs::remove_dir(path)
}

/// Add the graphics process role while retaining the image's imports, other
/// profiles and service settings. In particular, preserve Droidloom Home's
/// bootstrap import when supplied by an existing device image.
pub(crate) fn graphics_service(source: &str) -> io::Result<String> {
    fn finish(out: &mut String, profiles: &mut Vec<String>) {
        profiles.retain(|p| p != "HighPerformance" && p != "MaxPerformance");
        profiles.push("MaxPerformance".into());
        out.push_str(&format!("    task_profiles {}\n    setenv MESA_BACKGROUND_CPUSET /dev/cpuset/background/tasks\n", profiles.join(" ")));
        profiles.clear();
    }
    let mut result = String::new();
    let mut active = false;
    let mut services = 0;
    let mut profiles = Vec::new();
    for line in source.lines() {
        let tokens: Vec<_> = line.split_whitespace().collect();
        let directive =
            !line.starts_with(char::is_whitespace) && !tokens.is_empty() && !line.starts_with('#');
        if active && directive {
            finish(&mut result, &mut profiles);
            active = false;
        }
        if tokens.get(0) == Some(&"service") && tokens.get(1) == Some(&"surfaceflinger") {
            active = true;
            services += 1;
        }
        if active && tokens.first() == Some(&"task_profiles") {
            profiles = tokens[1..].iter().map(|s| (*s).to_string()).collect();
            continue;
        }
        if active
            && tokens.first() == Some(&"setenv")
            && tokens.get(1) == Some(&"MESA_BACKGROUND_CPUSET")
        {
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }
    if active {
        finish(&mut result, &mut profiles);
    }
    if services != 1 {
        return Err(io::Error::other(
            "expected exactly one SurfaceFlinger service",
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod service_tests {
    #[test]
    fn preserves_bootstrap_import_and_other_service_policies() {
        let input = "service surfaceflinger /system/bin/surfaceflinger\n    user system\n    task_profiles HighPerformance SomeMemoryPolicy\nimport /droidloom/ime/droidloom-home.rc\n";
        let output = super::graphics_service(input).unwrap();
        assert!(output.contains("task_profiles SomeMemoryPolicy MaxPerformance\n"));
        assert!(output.ends_with("import /droidloom/ime/droidloom-home.rc\n"));
        assert_eq!(output.matches("MESA_BACKGROUND_CPUSET").count(), 1);
        assert_eq!(super::graphics_service(&output).unwrap(), output);
    }
}
