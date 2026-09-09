//! Linux affinity and topology backend, independent of worker classification.
use crate::{Role, current};
use std::{fs, io, mem, sync::OnceLock};
/// A bounded Linux CPU set, serialized using the kernel's CPU-list notation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuSet(Vec<usize>);

impl CpuSet {
    /// Parse a nonempty kernel CPU list without accepting truncated masks.
    pub fn parse(value: &str) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid CPU list");
        let mut cpus = Vec::new();
        for part in value.trim().split(',') {
            let mut range = part.split('-');
            let start: usize = range
                .next()
                .ok_or_else(invalid)?
                .parse()
                .map_err(|_| invalid())?;
            let end = range
                .next()
                .map(str::parse)
                .transpose()
                .map_err(|_| invalid())?
                .unwrap_or(start);
            if range.next().is_some() || start > end || end >= libc::CPU_SETSIZE as usize {
                return Err(invalid());
            }
            cpus.extend(start..=end);
        }
        cpus.sort_unstable();
        cpus.dedup();
        if cpus.is_empty() {
            return Err(invalid());
        }
        Ok(Self(cpus))
    }

    /// Return a Linux CPU list.
    pub fn list(&self) -> String {
        self.0
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Read the calling thread's effective CPU mask.
    pub fn current() -> io::Result<Self> {
        // SAFETY: cpu_set_t is initialized writable storage with its exact size.
        let mut set: libc::cpu_set_t = unsafe { mem::zeroed() };
        if unsafe { libc::sched_getaffinity(0, mem::size_of_val(&set), &mut set) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let cpus = (0..libc::CPU_SETSIZE as usize)
            .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
            .collect();
        Ok(Self(cpus))
    }

    /// Apply only to this thread; never modifies another process or cgroup.
    pub fn apply(&self) -> io::Result<()> {
        // SAFETY: zeroed cpu_set_t and CPU indices validated at construction.
        let mut set: libc::cpu_set_t = unsafe { mem::zeroed() };
        for &cpu in &self.0 {
            unsafe { libc::CPU_SET(cpu, &mut set) };
        }
        if unsafe { libc::sched_setaffinity(0, mem::size_of_val(&set), &set) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let actual = Self::current()?;
        if actual.0.is_empty() || !actual.0.iter().all(|cpu| self.0.contains(cpu)) {
            return Err(io::Error::other(
                "kernel did not retain the requested CPU class",
            ));
        }
        Ok(())
    }
}

/// Topology, independent of the thread-role policy.
#[derive(Clone, Debug)]
pub struct Groups {
    /// Original CPU domain, never inferred from an already narrowed worker.
    pub normal: CpuSet,
    /// Every capacity tier above the smallest, including prime cores.
    pub graphics: CpuSet,
    /// Smallest capacity tier.
    pub background: CpuSet,
}

impl Groups {
    /// Validate explicit platform masks; no CPU numbers are device assumptions.
    pub fn new(normal: CpuSet, graphics: CpuSet, background: CpuSet) -> io::Result<Self> {
        let mut union = graphics.0.clone();
        union.extend(&background.0);
        union.sort_unstable();
        if union != normal.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "big and little masks must partition the inherited CPU domain",
            ));
        }
        Ok(Self {
            normal,
            graphics,
            background,
        })
    }

    /// Homogeneous CPUs need no placement policy. Incomplete data is an error.
    pub fn from_capacities(normal: CpuSet, values: &[(usize, u32)]) -> io::Result<Option<Self>> {
        let mut observed: Vec<_> = values.iter().map(|&(cpu, _)| cpu).collect();
        observed.sort_unstable();
        if observed != normal.0 || values.iter().any(|&(_, capacity)| capacity == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete CPU capacity data",
            ));
        }
        let min = values.iter().map(|&(_, v)| v).min().unwrap();
        if values.iter().all(|&(_, v)| v == min) {
            return Ok(None);
        }
        let mut big: Vec<_> = values
            .iter()
            .filter(|&&(_, v)| v > min)
            .map(|&(c, _)| c)
            .collect();
        let mut little: Vec<_> = values
            .iter()
            .filter(|&&(_, v)| v == min)
            .map(|&(c, _)| c)
            .collect();
        big.sort_unstable();
        little.sort_unstable();
        Self::new(normal, CpuSet(big), CpuSet(little)).map(Some)
    }

    /// Detect host topology or use a paired platform override. Never guesses by MHz.
    pub fn detect() -> io::Result<Option<Self>> {
        if std::env::var("DROIDLOOM_CPU_PLACEMENT").as_deref() == Ok("off") {
            return Ok(None);
        }
        let normal = CpuSet::current()?;
        match (
            std::env::var("DROIDLOOM_BIG_CPUS"),
            std::env::var("DROIDLOOM_LITTLE_CPUS"),
        ) {
            (Ok(big), Ok(little)) => {
                Self::new(normal, CpuSet::parse(&big)?, CpuSet::parse(&little)?).map(Some)
            }
            (Err(std::env::VarError::NotPresent), Err(std::env::VarError::NotPresent)) => {
                let values = normal
                    .0
                    .iter()
                    .map(|&cpu| {
                        let value = fs::read_to_string(format!(
                            "/sys/devices/system/cpu/cpu{cpu}/cpu_capacity"
                        ))?;
                        let value = value
                            .trim()
                            .parse()
                            .map_err(|_| io::Error::other("invalid CPU capacity"))?;
                        Ok((cpu, value))
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                Self::from_capacities(normal, &values)
            }
            _ => Err(io::Error::other(
                "DROIDLOOM_BIG_CPUS and DROIDLOOM_LITTLE_CPUS must be supplied together",
            )),
        }
    }

    /// Resolve a declared role.
    pub fn mask(&self, role: Role) -> &CpuSet {
        match role {
            Role::Graphics => &self.graphics,
            Role::Normal => &self.normal,
            Role::Background => &self.background,
        }
    }
}

static GROUPS: OnceLock<Option<Groups>> = OnceLock::new();

/// Initialize host placement once, before narrowing the creating thread.
pub fn initialize(role: Role) -> Option<&'static Groups> {
    let groups = GROUPS.get_or_init(|| match Groups::detect() {
        Ok(groups) => {
            if let Some(ref g) = groups {
                eprintln!("Droidloom CPU placement: big={} little={} normal={}", g.graphics.list(), g.background.list(), g.normal.list());
            }
            groups
        }
        Err(error) => {
            eprintln!("Droidloom CPU placement unavailable: {error}; retaining inherited masks (a platform may supply DROIDLOOM_BIG_CPUS and DROIDLOOM_LITTLE_CPUS)");
            None
        }
    });
    current(role);
    groups.as_ref()
}

pub(super) fn apply_current(role: Role) -> io::Result<()> {
    match GROUPS.get() {
        Some(Some(groups)) => groups.mask(role).apply(),
        _ => Ok(()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    #[test]
    fn validates_topology_and_platform_partitions() {
        for invalid in ["", "-1", "1-0", "1-2-3", "1,", "1024"] {
            assert!(CpuSet::parse(invalid).is_err());
        }
        let all = CpuSet::parse("0-3").unwrap();
        let g = Groups::from_capacities(all.clone(), &[(0, 429), (1, 429), (2, 854), (3, 1024)])
            .unwrap()
            .unwrap();
        assert_eq!(g.graphics.list(), "2,3");
        assert_eq!(g.background.list(), "0,1");
        assert!(Groups::from_capacities(all.clone(), &[(0, 429)]).is_err());
        assert!(
            Groups::from_capacities(all.clone(), &[(0, 10), (1, 10), (2, 10), (3, 10)])
                .unwrap()
                .is_none()
        );
        assert!(
            Groups::new(
                all,
                CpuSet::parse("0-2").unwrap(),
                CpuSet::parse("2-3").unwrap()
            )
            .is_err()
        );
    }
    #[test]
    fn kernel_inheritance_worker_override_and_restore() {
        thread::spawn(|| {
            let original = CpuSet::current().unwrap();
            if original.0.len() < 2 {
                return;
            }
            let big = CpuSet(vec![original.0[0]]);
            let little = CpuSet(vec![original.0[1]]);
            big.apply().unwrap();
            let expected = big.clone();
            thread::spawn(move || {
                assert_eq!(CpuSet::current().unwrap(), expected);
                little.apply().unwrap();
                assert_eq!(CpuSet::current().unwrap(), little);
            })
            .join()
            .unwrap();
            assert_eq!(CpuSet::current().unwrap(), big);
            let output = command_in_domain("sh", &original)
                .args([
                    "-c",
                    "awk '/Cpus_allowed_list:/ {print $2}' /proc/self/status",
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(
                CpuSet::parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap(),
                original
            );
            assert_eq!(
                CpuSet::current().unwrap(),
                big,
                "child restore changed creator"
            );
            original.apply().unwrap();
            assert_eq!(CpuSet::current().unwrap(), original);
        })
        .join()
        .unwrap();
    }
}

/// Construct a child command that restores the original application CPU domain.
/// The post-fork hook performs one syscall: no allocation, locks or discovery.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    match GROUPS.get() {
        Some(Some(groups)) => command_in_domain(program, &groups.normal),
        _ => std::process::Command::new(program),
    }
}

fn command_in_domain(program: impl AsRef<std::ffi::OsStr>, cpus: &CpuSet) -> std::process::Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: a validated CpuSet is converted before fork into initialized
    // fixed-size storage. The child callback only uses that captured storage.
    let mut mask: libc::cpu_set_t = unsafe { mem::zeroed() };
    for &cpu in &cpus.0 {
        unsafe { libc::CPU_SET(cpu, &mut mask) };
    }
    let mut command = std::process::Command::new(program);
    unsafe {
        command.pre_exec(move || {
            if libc::sched_setaffinity(0, mem::size_of_val(&mask), &mask) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}
