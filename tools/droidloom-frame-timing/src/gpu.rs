//! Read-only DRM client accounting, independent of frame tracing and its cap.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    path::Path,
    time::Instant,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClientKey {
    pub driver: String,
    pub device: Option<String>,
    pub client_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    pub label: String,
    pub start_ticks: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Client {
    pub key: ClientKey,
    pub owners: BTreeSet<Process>,
    pub handles: BTreeSet<(u32, u32)>,
    pub engine_ns: BTreeMap<String, u64>,
    pub capacity: BTreeMap<String, u64>,
    pub resident_bytes: BTreeMap<String, u64>,
    pub shared_bytes: BTreeMap<String, u64>,
}

fn quantity(value: &str, units: &[(&str, u64)]) -> Result<u64> {
    let mut parts = value.split_whitespace();
    let number: u64 = parts.next().ok_or("missing counter")?.parse()?;
    let unit = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return Err("unexpected counter suffix".into());
    }
    let multiplier = units
        .iter()
        .find(|(name, _)| *name == unit)
        .ok_or("unsupported counter unit")?
        .1;
    number
        .checked_mul(multiplier)
        .ok_or_else(|| "counter overflow".into())
}

pub fn parse_fdinfo(text: &str) -> Result<Option<Client>> {
    let fields: BTreeMap<_, _> = text
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();
    let Some(driver) = fields.get("drm-driver") else {
        return Ok(None);
    };
    let id = fields
        .get("drm-client-id")
        .ok_or("DRM driver has no client ID; cannot deduplicate")?
        .parse()?;
    let mut client = Client {
        key: ClientKey {
            driver: (*driver).into(),
            device: fields.get("drm-pdev").map(|v| (*v).into()),
            client_id: id,
        },
        owners: BTreeSet::new(),
        handles: BTreeSet::new(),
        engine_ns: BTreeMap::new(),
        capacity: BTreeMap::new(),
        resident_bytes: BTreeMap::new(),
        shared_bytes: BTreeMap::new(),
    };
    for (key, value) in fields {
        if let Some(engine) = key.strip_prefix("drm-engine-capacity-") {
            let n = quantity(value, &[("", 1)])?;
            if n == 0 {
                return Err("zero engine capacity".into());
            }
            client.capacity.insert(engine.into(), n);
        } else if let Some(engine) = key.strip_prefix("drm-engine-") {
            client
                .engine_ns
                .insert(engine.into(), quantity(value, &[("ns", 1)])?);
        } else if let Some(region) = key.strip_prefix("drm-resident-") {
            client.resident_bytes.insert(
                region.into(),
                quantity(value, &[("", 1), ("KiB", 1024), ("MiB", 1024 * 1024)])?,
            );
        } else if let Some(region) = key.strip_prefix("drm-shared-") {
            client.shared_bytes.insert(
                region.into(),
                quantity(value, &[("", 1), ("KiB", 1024), ("MiB", 1024 * 1024)])?,
            );
        }
        // drm-memory-* aliases resident on AMD: never add it a second time.
        // Cycles-only drivers are retained but do not acquire invented ns values.
    }
    Ok(Some(client))
}

pub fn process(root: &Path, pid: u32, label: String) -> Result<Process> {
    let text = fs::read_to_string(root.join(pid.to_string()).join("stat"))?;
    let start_ticks = text
        .rsplit_once(") ")
        .ok_or("invalid process stat")?
        .1
        .split_whitespace()
        .nth(19)
        .ok_or("missing process start time")?
        .parse()?;
    Ok(Process {
        pid,
        label,
        start_ticks,
    })
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub elapsed_ns: u64,
    pub scan_ns: u64,
    pub clients: Vec<Client>,
    pub issues: Vec<String>,
}

fn merge_max(target: &mut BTreeMap<String, u64>, other: BTreeMap<String, u64>) {
    for (name, value) in other {
        target
            .entry(name)
            .and_modify(|old| *old = (*old).max(value))
            .or_insert(value);
    }
}

fn merge(clients: &mut BTreeMap<ClientKey, Client>, client: Client) {
    if let Some(old) = clients.get_mut(&client.key) {
        old.owners.extend(client.owners);
        old.handles.extend(client.handles);
        merge_max(&mut old.engine_ns, client.engine_ns);
        merge_max(&mut old.capacity, client.capacity);
        merge_max(&mut old.resident_bytes, client.resident_bytes);
        merge_max(&mut old.shared_bytes, client.shared_bytes);
    } else {
        clients.insert(client.key.clone(), client);
    }
}

pub fn snapshot(root: &Path, processes: &[Process], epoch: Instant) -> Snapshot {
    let start = Instant::now();
    let mut result = Snapshot::default();
    let mut clients = BTreeMap::new();
    for expected in processes {
        let current = process(root, expected.pid, expected.label.clone());
        if !matches!(current, Ok(ref p) if p == expected) {
            result.issues.push(format!(
                "{} (PID {}): exited, inaccessible or process identity changed",
                expected.label, expected.pid
            ));
            continue;
        }
        let dir = root.join(expected.pid.to_string());
        let entries = match fs::read_dir(dir.join("fdinfo")) {
            Ok(entries) => entries,
            Err(e) => {
                result
                    .issues
                    .push(format!("PID {} fdinfo: {e}", expected.pid));
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                result
                    .issues
                    .push(format!("PID {}: unreadable fdinfo entry", expected.pid));
                continue;
            };
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            // Avoid reading every socket/pipe fdinfo; standard DRM nodes are /dev/dri/*.
            let link = match fs::read_link(dir.join("fd").join(fd.to_string())) {
                Ok(link) => link,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    result
                        .issues
                        .push(format!("PID {} fd {fd}: {e}", expected.pid));
                    continue;
                }
            };
            if !link.starts_with("/dev/dri") {
                continue;
            }
            let read = || -> Result<Option<Client>> {
                let mut text = String::new();
                fs::File::open(entry.path())?
                    .take(65_537)
                    .read_to_string(&mut text)?;
                if text.len() > 65_536 {
                    return Err("oversized fdinfo".into());
                }
                parse_fdinfo(&text)
            };
            match read() {
                Ok(Some(mut client)) => {
                    client.owners.insert(expected.clone());
                    client.handles.insert((expected.pid, fd));
                    merge(&mut clients, client);
                }
                Ok(None) => result.issues.push(format!(
                    "PID {} DRM fd {fd}: no standard DRM accounting",
                    expected.pid
                )),
                Err(e) => result
                    .issues
                    .push(format!("PID {} DRM fd {fd}: {e}", expected.pid)),
            }
        }
        if !matches!(process(root, expected.pid, expected.label.clone()), Ok(ref p) if p == expected)
        {
            result.issues.push(format!(
                "PID {} changed during scan; discarded its handles",
                expected.pid
            ));
            for client in clients.values_mut() {
                client.owners.remove(expected);
                client.handles.retain(|(pid, _)| *pid != expected.pid);
            }
            clients.retain(|_, c| !c.owners.is_empty());
        }
    }
    result.clients = clients.into_values().collect();
    result.elapsed_ns = epoch.elapsed().as_nanos().try_into().unwrap_or(u64::MAX);
    result.scan_ns = start.elapsed().as_nanos().try_into().unwrap_or(u64::MAX);
    result
}

pub fn discover(root: &Path) -> Result<(Vec<Process>, usize)> {
    let mut processes = Vec::new();
    let mut inaccessible = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        // Names only; do not collect command arguments, environments or app contents.
        // argv[0] can be rewritten to contain an entire command line. Linux
        // comm provides a name without collecting arguments or environment.
        let name = fs::read_to_string(dir.join("comm"))
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|_| "unknown".into());
        match process(root, pid, name) {
            Ok(p) => processes.push(p),
            Err(_) => inaccessible += 1,
        }
    }
    Ok((processes, inaccessible))
}

#[derive(Debug, Serialize)]
pub struct EngineUsage {
    pub client: ClientKey,
    pub owners: BTreeSet<Process>,
    pub engine: String,
    pub capacity: u64,
    pub busy_ns: u64,
    pub covered_ns: u64,
    pub intervals: usize,
    pub regressions: usize,
    pub discontinuities: usize,
    pub capacity_percent: Option<f64>,
}

pub fn summarize(samples: &[Snapshot]) -> Vec<EngineUsage> {
    // High-water marks implement the kernel's documented nonmonotonic-counter rule.
    let mut rows: BTreeMap<(ClientKey, String, u64), EngineUsage> = BTreeMap::new();
    let mut previous: BTreeMap<(ClientKey, String), (u64, u64, usize, u64)> = BTreeMap::new();
    for (index, sample) in samples.iter().enumerate() {
        for client in &sample.clients {
            for (engine, &counter) in &client.engine_ns {
                let capacity = client.capacity.get(engine).copied().unwrap_or(1);
                let row = rows
                    .entry((client.key.clone(), engine.clone(), capacity))
                    .or_insert_with(|| EngineUsage {
                        client: client.key.clone(),
                        owners: BTreeSet::new(),
                        engine: engine.clone(),
                        capacity,
                        busy_ns: 0,
                        covered_ns: 0,
                        intervals: 0,
                        regressions: 0,
                        discontinuities: 0,
                        capacity_percent: None,
                    });
                row.owners.extend(client.owners.clone());
                let state = previous
                    .entry((client.key.clone(), engine.clone()))
                    .or_insert((counter, sample.elapsed_ns, index, capacity));
                if state.2 == index {
                    continue;
                }
                if state.2 + 1 == index && state.3 == capacity && sample.elapsed_ns > state.1 {
                    row.busy_ns += counter.saturating_sub(state.0);
                    row.covered_ns += sample.elapsed_ns - state.1;
                    row.intervals += 1;
                    row.regressions += usize::from(counter < state.0);
                } else {
                    row.discontinuities += 1;
                }
                *state = (counter.max(state.0), sample.elapsed_ns, index, capacity);
            }
        }
    }
    for row in rows.values_mut() {
        if row.covered_ns > 0 {
            row.capacity_percent =
                Some(row.busy_ns as f64 / row.covered_ns as f64 / row.capacity as f64 * 100.);
        }
    }
    rows.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn client(id: u64, ns: u64) -> Client {
        parse_fdinfo(&format!("drm-driver: amdgpu\ndrm-pdev: 0000:0a:00.0\ndrm-client-id: {id}\ndrm-engine-gfx: {ns} ns\n")).unwrap().unwrap()
    }
    fn sample(time: u64, clients: Vec<Client>) -> Snapshot {
        Snapshot {
            elapsed_ns: time,
            clients,
            ..Default::default()
        }
    }
    #[test]
    fn shared_handles_are_one_client_and_devices_remain_distinct() {
        let mut clients = BTreeMap::new();
        let mut a = client(7, 100);
        a.owners.insert(Process {
            pid: 10,
            label: "app".into(),
            start_ticks: 1,
        });
        a.handles.insert((10, 3));
        let mut b = a.clone();
        b.engine_ns.insert("gfx".into(), 110);
        b.owners.insert(Process {
            pid: 11,
            label: "child".into(),
            start_ticks: 2,
        });
        b.handles.insert((11, 4));
        merge(&mut clients, a);
        merge(&mut clients, b);
        assert_eq!(clients.len(), 1);
        let c = clients.values().next().unwrap();
        assert_eq!(c.engine_ns["gfx"], 110);
        assert_eq!(c.owners.len(), 2);
        assert_eq!(c.handles.len(), 2);
        let mut other = client(7, 500);
        other.key.device = Some("0000:04:00.0".into());
        merge(&mut clients, other);
        assert_eq!(clients.len(), 2);
    }
    #[test]
    fn capacity_and_memory_alias_are_not_engine_or_duplicate_memory() {
        let c = parse_fdinfo("drm-driver: i915\ndrm-client-id: 9\ndrm-engine-render: 50 ns\ndrm-engine-capacity-render: 2\ndrm-resident-system: 3 MiB\ndrm-memory-system: 999 KiB\ndrm-shared-system: 4 KiB\n").unwrap().unwrap();
        assert_eq!(c.engine_ns.len(), 1);
        assert_eq!(c.capacity["render"], 2);
        assert_eq!(c.resident_bytes["system"], 3 * 1024 * 1024);
        assert_eq!(c.shared_bytes["system"], 4096);
        let mut end = c.clone();
        end.engine_ns.insert("render".into(), 150);
        let rows = summarize(&[sample(1, vec![c]), sample(101, vec![end])]);
        assert_eq!(rows[0].capacity_percent, Some(50.));
    }
    #[test]
    fn regressing_counters_hold_high_water_until_catchup() {
        let rows = summarize(&[
            sample(0, vec![client(1, 100)]),
            sample(100, vec![client(1, 90)]),
            sample(200, vec![client(1, 95)]),
            sample(300, vec![client(1, 130)]),
        ]);
        assert_eq!(rows[0].busy_ns, 30);
        assert_eq!(rows[0].regressions, 2);
        assert_eq!(rows[0].covered_ns, 300);
    }
    #[test]
    fn missing_clients_and_new_clients_have_no_invented_zero_intervals() {
        let rows = summarize(&[
            sample(0, vec![client(1, 100)]),
            sample(100, vec![]),
            sample(200, vec![client(1, 130), client(2, 50)]),
        ]);
        assert!(
            rows.iter()
                .all(|r| r.capacity_percent.is_none() && r.intervals == 0)
        );
        assert_eq!(rows[0].discontinuities, 1);
    }
    #[test]
    fn unsupported_units_missing_ids_and_overflows_fail_closed() {
        for text in [
            "drm-driver: amdgpu\n",
            "drm-driver: amdgpu\ndrm-client-id: 1\ndrm-engine-gfx: 1 cycles",
            "drm-driver: amdgpu\ndrm-client-id: 1\ndrm-resident-vram: 18446744073709551615 MiB",
            "drm-driver: amdgpu\ndrm-client-id: 1\ndrm-engine-capacity-gfx: 0",
        ] {
            assert!(parse_fdinfo(text).is_err());
        }
        assert!(parse_fdinfo("pos: 0\nflags: 0100002").unwrap().is_none());
        let c = parse_fdinfo(
            "drm-driver: xe\ndrm-client-id: 1\ndrm-cycles-render: 9\ndrm-total-cycles-render: 100",
        )
        .unwrap()
        .unwrap();
        assert!(c.engine_ns.is_empty());
    }
    #[test]
    fn pid_reuse_is_detected_even_if_the_name_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("10");
        fs::create_dir(&dir).unwrap();
        let write_stat = |start| {
            fs::write(
                dir.join("stat"),
                format!(
                    "10 (name with ) spaces) S {} {start} 0\n",
                    vec!["0"; 18].join(" ")
                ),
            )
            .unwrap()
        };
        write_stat(5);
        let old = process(tmp.path(), 10, "app".into()).unwrap();
        assert_eq!(old.start_ticks, 5);
        write_stat(6);
        let snap = snapshot(tmp.path(), &[old], Instant::now());
        assert!(snap.clients.is_empty());
        assert!(snap.issues[0].contains("identity changed"));
    }

    #[test]
    fn discovery_uses_comm_and_sampler_deduplicates_cross_process_handles() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        for pid in [10, 11] {
            let dir = tmp.path().join(pid.to_string());
            fs::create_dir_all(dir.join("fdinfo")).unwrap();
            fs::create_dir(dir.join("fd")).unwrap();
            fs::write(
                dir.join("stat"),
                format!("{pid} (safe_name) S {} 5 0\n", vec!["0"; 18].join(" ")),
            )
            .unwrap();
            fs::write(dir.join("comm"), "safe_name\n").unwrap();
            fs::write(
                dir.join("cmdline"),
                "rewritten argv with private arguments\0",
            )
            .unwrap();
            for fd in [3, 4] {
                // An intentionally nonexistent node: sampling must never open it.
                symlink(
                    "/dev/dri/does-not-exist",
                    dir.join("fd").join(fd.to_string()),
                )
                .unwrap();
                fs::write(
                    dir.join("fdinfo").join(fd.to_string()),
                    "drm-driver: amdgpu\ndrm-client-id: 9\ndrm-engine-gfx: 100 ns\n",
                )
                .unwrap();
            }
        }
        let (processes, inaccessible) = discover(tmp.path()).unwrap();
        assert_eq!(inaccessible, 0);
        assert!(processes.iter().all(|p| p.label == "safe_name"));
        let snap = snapshot(tmp.path(), &processes, Instant::now());
        assert!(snap.issues.is_empty());
        assert_eq!(snap.clients.len(), 1);
        assert_eq!(snap.clients[0].owners.len(), 2);
        assert_eq!(snap.clients[0].handles.len(), 4);
        assert_eq!(snap.clients[0].engine_ns["gfx"], 100);
    }
}
