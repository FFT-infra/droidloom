//! Linux utilization clamps, driver-queue discovery and CPU capacities.
use crate::{Actuator, ApplyError, UTIL_MAX};
use std::{fs, io, mem};

// Not exported by Android's libc crate.
const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;
/// `sched_util_min` value that restores the system default.
const CLAMP_RESET: u32 = u32::MAX;

#[repr(C)]
#[derive(Default)]
struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
    sched_util_min: u32,
    sched_util_max: u32,
}

/// Applies floors with `sched_setattr`. Policy, priority and reset-on-fork
/// stay as the thread's owner set them.
pub struct Scheduler {
    rt_default: u32,
}

impl Scheduler {
    /// Read the real-time default once; it is a boot-time platform setting.
    pub fn new() -> Self {
        let rt_default = fs::read_to_string("/proc/sys/kernel/sched_util_clamp_min_rt_default")
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(UTIL_MAX);
        Self { rt_default }
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Actuator for Scheduler {
    fn apply(&mut self, tid: i32, floor: u32) -> Result<(), ApplyError> {
        // SAFETY: plain syscall on a thread ID; no memory is passed.
        let policy = unsafe { libc::sched_getscheduler(tid) };
        if policy < 0 {
            return Err(error(io::Error::last_os_error()));
        }
        let minimum = match policy & !libc::SCHED_RESET_ON_FORK {
            // Deadline threads are scheduled by their reservation.
            libc::SCHED_DEADLINE => return Ok(()),
            // A real-time thread without its own clamp runs at the RT default;
            // a session must never lower it.
            libc::SCHED_FIFO | libc::SCHED_RR if floor <= self.rt_default => CLAMP_RESET,
            _ if floor == 0 => CLAMP_RESET,
            _ => floor.min(UTIL_MAX),
        };
        let attr = SchedAttr {
            size: mem::size_of::<SchedAttr>() as u32,
            sched_flags: SCHED_FLAG_KEEP_POLICY
                | SCHED_FLAG_KEEP_PARAMS
                | SCHED_FLAG_UTIL_CLAMP_MIN,
            sched_util_min: minimum,
            ..SchedAttr::default()
        };
        // SAFETY: attr is an initialized sched_attr whose size field matches it.
        let result = unsafe { libc::syscall(libc::SYS_sched_setattr, tid, &raw const attr, 0) };
        if result == 0 {
            Ok(())
        } else {
            Err(error(io::Error::last_os_error()))
        }
    }

    fn driver_threads(&mut self, tgid: i32) -> Vec<i32> {
        let Ok(entries) = fs::read_dir(format!("/proc/{tgid}/task")) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let tid: i32 = entry.file_name().to_str()?.parse().ok()?;
                let name = fs::read_to_string(entry.path().join("comm")).ok()?;
                driver_queue(name.trim_end()).then_some(tid)
            })
            .collect()
    }
}

/// Mesa names its gallium driver queue "<process>:gdrvN".
fn driver_queue(name: &str) -> bool {
    name.contains(":gdrv")
}

fn error(error: io::Error) -> ApplyError {
    if error.raw_os_error() == Some(libc::ESRCH) {
        ApplyError::Gone
    } else {
        ApplyError::Other(error)
    }
}

/// Each CPU's `cpu_capacity`. Empty when the kernel does not report one,
/// which [`crate::Policy::from_capacities`] treats as homogeneous.
pub fn cpu_capacities() -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let index = name.to_str()?.strip_prefix("cpu")?;
            if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            fs::read_to_string(entry.path().join("cpu_capacity"))
                .ok()?
                .trim()
                .parse()
                .ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn util_min(tid: i32) -> u32 {
        let mut attr = SchedAttr::default();
        let size = mem::size_of::<SchedAttr>() as u32;
        // SAFETY: attr is writable storage of the advertised size.
        let result = unsafe { libc::syscall(libc::SYS_sched_getattr, tid, &raw mut attr, size, 0) };
        assert_eq!(result, 0, "{}", io::Error::last_os_error());
        attr.sched_util_min
    }

    #[test]
    fn names_only_gallium_driver_queues() {
        assert!(driver_queue("org.tele:gdrv0"));
        assert!(!driver_queue("org.tele:ir3q0"));
        assert!(!driver_queue("RenderThread"));
        assert!(!driver_queue("gdrv"));
    }

    #[test]
    fn own_thread_floor_applies_and_resets_without_privilege() {
        std::thread::spawn(|| {
            // SAFETY: gettid has no preconditions.
            let tid = unsafe { libc::gettid() };
            let mut scheduler = Scheduler::new();
            if let Err(ApplyError::Other(error)) = scheduler.apply(tid, 300) {
                // Kernels without utilization clamping reject the flag.
                assert_eq!(error.raw_os_error(), Some(libc::EOPNOTSUPP), "{error}");
                return;
            }
            assert_eq!(util_min(tid), 300);
            assert!(scheduler.apply(tid, 0).is_ok());
            assert_eq!(util_min(tid), 0);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn exited_threads_are_gone() {
        let mut scheduler = Scheduler::new();
        assert!(matches!(
            scheduler.apply(i32::MAX, 300),
            Err(ApplyError::Gone)
        ));
    }
}
