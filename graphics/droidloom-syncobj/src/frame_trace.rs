//! Opt-in frame correlation. The render thread never waits for the log writer.
//! Times are CLOCK_MONOTONIC nanoseconds shared by host and Android, not wall time.

use std::io::{self, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::{OnceLock, atomic::{AtomicU64, Ordering}, mpsc::{self, SyncSender}};

const FIELDS: usize = 16;
const MAX_RECORDS: u64 = 65_536;
const MAX_FENCES: usize = 64;

struct Record {
    stage: &'static str,
    object: u64,
    frame: u64,
    buffer: u64,
    observed_ns: u64,
    values: [(&'static str, u64); FIELDS],
    count: usize,
    sequence: u64,
}

struct Trace {
    sender: SyncSender<Record>,
    sequence: AtomicU64,
    dropped: AtomicU64,
}

static TRACE: OnceLock<Option<Trace>> = OnceLock::new();

fn trace() -> Option<&'static Trace> {
    TRACE.get_or_init(|| {
        if std::env::var_os("DROIDLOOM_FRAME_TRACE").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return None;
        }
        let (sender, receiver) = mpsc::sync_channel::<Record>(256);
        std::thread::Builder::new().name("dl-frame-log".into()).spawn(move || {
            droidloom_cpu_placement::current(droidloom_cpu_placement::Role::Background);
            for record in receiver {
                let mut line = format!("Droidloom frame trace: pid={} seq={} stage={} object={} frame={} buffer={} observed_ns={}",
                    std::process::id(), record.sequence, record.stage, record.object,
                    record.frame, record.buffer, record.observed_ns);
                for (key, value) in &record.values[..record.count] {
                    use std::fmt::Write;
                    let _ = write!(line, " {key}={value}");
                }
                let dropped = TRACE.get().and_then(Option::as_ref)
                    .map_or(0, |trace| trace.dropped.load(Ordering::Relaxed));
                {
                    use std::fmt::Write;
                    let _ = writeln!(line, " dropped_total={dropped}");
                }
                // Android stdio_to_kmsg treats each write as a separate record.
                // Format everything first so the loss counter stays on its event.
                if io::stderr().lock().write_all(line.as_bytes()).is_err() { break; }
            }
        }).ok()?;
        Some(Trace { sender, sequence: AtomicU64::new(0), dropped: AtomicU64::new(0) })
    }).as_ref()
}

/// Whether tracing was explicitly enabled for this process at startup.
pub fn enabled() -> bool { trace().is_some() }

/// Deterministic sampling shared by Android and the host; IDs are task-scoped.
pub fn sampled(frame: u64) -> bool { frame % 8 == 1 && enabled() }

/// Monotonic timestamp, or zero if the clock cannot be read. Zero is unknown.
pub fn now_ns() -> u64 {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: time is writable storage for one timespec; no pointer is retained.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 { return 0; }
    u64::try_from(time.tv_sec).ok().and_then(|s| s.checked_mul(1_000_000_000))
        .and_then(|s| u64::try_from(time.tv_nsec).ok().and_then(|n| s.checked_add(n))).unwrap_or(0)
}

/// Queue a sampled event without blocking; sequence gaps and dropped_total flag loss.
pub fn event(stage: &'static str, object: u64, frame: u64, buffer: u64,
             values: &[(&'static str, u64)]) {
    if !sampled(frame) { return; }
    let Some(trace) = trace() else { return; };
    let sequence = trace.sequence.fetch_add(1, Ordering::Relaxed) + 1;
    if sequence > MAX_RECORDS { return; }
    let mut record = Record { stage, object, frame, buffer, observed_ns: now_ns(),
        values: [("", 0); FIELDS], count: values.len().min(FIELDS), sequence };
    record.values[..record.count].copy_from_slice(&values[..record.count]);
    if trace.sender.try_send(record).is_err() { trace.dropped.fetch_add(1, Ordering::Relaxed); }
}

#[repr(C)]
#[derive(Default)]
struct FileInfo {
    name: [u8; 32], status: i32, flags: u32, num_fences: u32, pad: u32, fences: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FenceInfo {
    object_name: [u8; 32], driver_name: [u8; 32], status: i32, flags: u32, timestamp_ns: u64,
}

/// Nonblocking SYNC_IOC_FILE_INFO snapshot. None means pending or no real timestamp.
/// The maximum component signal time is when a merged fence completes.
pub fn fence_signal_ns(fd: BorrowedFd<'_>) -> io::Result<Option<u64>> {
    const REQUEST: u64 = (3 << 30) | ((size_of::<FileInfo>() as u64) << 16) | ((b'>' as u64) << 8) | 4;
    let mut info = FileInfo::default();
    // SAFETY: Linux sync_file UAPI uses this exact repr(C) struct. With count zero
    // the kernel only returns the count; all pointers remain valid during ioctl.
    if unsafe { libc::ioctl(fd.as_raw_fd(), REQUEST as super::IoctlRequest, &mut info) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if info.status < 0 { return Err(io::Error::other("sync-file completed with an error")); }
    if info.status == 0 { return Ok(None); }
    if info.num_fences == 0 || info.num_fences as usize > MAX_FENCES {
        return Err(io::Error::other("sync-file component count outside trace bound"));
    }
    let mut fences = [FenceInfo { object_name: [0; 32], driver_name: [0; 32], status: 0,
        flags: 0, timestamp_ns: 0 }; MAX_FENCES];
    info.fences = fences.as_mut_ptr() as usize as u64;
    // SAFETY: the kernel writes at most the declared count, bounded by the live array.
    if unsafe { libc::ioctl(fd.as_raw_fd(), REQUEST as super::IoctlRequest, &mut info) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if info.num_fences as usize > MAX_FENCES { return Err(io::Error::other("sync-file count changed")); }
    summarize_fences(info.status, &fences[..info.num_fences as usize])
}

fn summarize_fences(status: i32, fences: &[FenceInfo]) -> io::Result<Option<u64>> {
    if status < 0 || fences.iter().any(|f| f.status < 0) {
        return Err(io::Error::other("sync-file component error"));
    }
    if status == 0 || fences.is_empty() || fences.iter().any(|f| f.status == 0 || f.timestamp_ns == 0) {
        return Ok(None);
    }
    // DRM may substitute its global already-signalled stub when exporting a
    // completed timeline point. Its timestamp is from early boot, not this frame.
    let timestamp = fences.iter().filter(|f| !f.driver_name.starts_with(b"stub\0"))
        .map(|f| f.timestamp_ns).max();
    timestamp.map(Some).ok_or_else(|| io::Error::other("placeholder fence has no GPU timestamp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fence(status: i32, timestamp_ns: u64) -> FenceInfo {
        FenceInfo { object_name: [0; 32], driver_name: [0; 32], status, flags: 0, timestamp_ns }
    }
    #[test]
    fn merged_fence_uses_last_completion_and_never_invents_missing_time() {
        assert_eq!(summarize_fences(1, &[fence(1, 9), fence(1, 20)]).unwrap(), Some(20));
        assert_eq!(summarize_fences(0, &[fence(1, 9), fence(0, 0)]).unwrap(), None);
        assert_eq!(summarize_fences(1, &[fence(1, 0)]).unwrap(), None);
        assert!(summarize_fences(-5, &[fence(-5, 20)]).is_err());
        let mut stub = fence(1, 999);
        stub.driver_name[..5].copy_from_slice(b"stub\0");
        assert!(summarize_fences(1, &[stub]).is_err());
        assert_eq!(summarize_fences(1, &[stub, fence(1, 20)]).unwrap(), Some(20));
    }
    #[test]
    fn sync_file_uapi_layout() {
        assert_eq!(size_of::<FileInfo>(), 56);
        assert_eq!(size_of::<FenceInfo>(), 80);
    }
}
