//! Opt-in frame correlation. The render thread never waits for the log writer.
//! Times are CLOCK_MONOTONIC nanoseconds shared by host and Android, not wall time.

use std::io;
mod clock;
#[cfg(not(target_os = "android"))]
use std::io::Write;
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
    mpsc::{self, SyncSender},
};

const FIELDS: usize = 16;
const MAX_RECORDS: u64 = 65_536;

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
    #[cfg(test)]
    written: AtomicU64,
}

static TRACE: OnceLock<Option<Trace>> = OnceLock::new();

fn trace() -> Option<&'static Trace> {
    TRACE.get_or_init(|| {
        if !requested() {
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
                if write_record(&line).is_err() { break; }
                #[cfg(test)]
                if let Some(Some(trace)) = TRACE.get() {
                    trace.written.fetch_add(1, Ordering::Release);
                }
            }
        }).ok()?;
        Some(Trace { sender, sequence: AtomicU64::new(0), dropped: AtomicU64::new(0),
                     #[cfg(test)] written: AtomicU64::new(0) })
    }).as_ref()
}

fn write_record(line: &str) -> io::Result<()> {
    #[cfg(not(target_os = "android"))]
    {
        io::stderr().lock().write_all(line.as_bytes())
    }
    #[cfg(target_os = "android")]
    {
        unsafe extern "C" {
            fn __android_log_write(
                priority: core::ffi::c_int,
                tag: *const core::ffi::c_char,
                message: *const core::ffi::c_char,
            ) -> core::ffi::c_int;
        }
        let line = std::ffi::CString::new(line).map_err(io::Error::other)?;
        // SAFETY: both strings are NUL terminated and live through the call;
        // liblog copies their contents. No application content is recorded.
        unsafe {
            __android_log_write(4, c"DroidloomTiming".as_ptr(), line.as_ptr());
        }
        Ok(())
    }
}

/// Whether tracing was explicitly enabled for this process at startup.
pub fn enabled() -> bool {
    trace().is_some()
}

/// Deterministic sampling shared by Android and the host; IDs are task-scoped.
pub fn sampled(frame: u64) -> bool {
    frame % 8 == 1 && enabled()
}

/// Monotonic timestamp, or zero if the clock cannot be read. Zero is unknown.
pub fn now_ns() -> u64 {
    clock_ns(clock::CLOCK_MONOTONIC)
}

fn clock_ns(clock: clock::ClockId) -> u64 {
    let mut time = clock::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time is writable storage for one timespec; no pointer is retained.
    if unsafe { clock::clock_gettime(clock, &mut time) } != 0 {
        return 0;
    }
    u64::try_from(time.tv_sec)
        .ok()
        .and_then(|s| s.checked_mul(1_000_000_000))
        .and_then(|s| {
            u64::try_from(time.tv_nsec)
                .ok()
                .and_then(|n| s.checked_add(n))
        })
        .unwrap_or(0)
}

fn requested() -> bool {
    if std::env::var_os("DROIDLOOM_FRAME_TRACE").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return true;
    }
    #[cfg(target_os = "android")]
    {
        unsafe extern "C" {
            fn __system_property_get(
                name: *const core::ffi::c_char,
                value: *mut core::ffi::c_char,
            ) -> core::ffi::c_int;
        }
        let mut value = [0 as core::ffi::c_char; 92];
        // SAFETY: Android's property API writes at most PROP_VALUE_MAX (92)
        // bytes; the name is NUL terminated and no pointer is retained.
        return unsafe {
            __system_property_get(
                c"persist.vendor.droidloom.frame_timing".as_ptr(),
                value.as_mut_ptr(),
            ) == 1
                && value[0] == b'1' as core::ffi::c_char
        };
    }
    #[cfg(not(target_os = "android"))]
    false
}

/// Clock pair for one thread; zero means disabled, unsampled or unavailable.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Stamp {
    /// CLOCK_MONOTONIC nanoseconds.
    pub wall_ns: u64,
    /// CLOCK_THREAD_CPUTIME_ID nanoseconds, including syscalls on this thread.
    pub cpu_ns: u64,
}

impl Stamp {
    fn start(frame: u64) -> Self {
        if !sampled(frame) {
            return Self::default();
        }
        Self {
            wall_ns: now_ns(),
            cpu_ns: clock_ns(clock::CLOCK_THREAD_CPUTIME_ID),
        }
    }

    fn elapsed(self, end: Self) -> Option<(u64, u64)> {
        if self.wall_ns == 0 || self.cpu_ns == 0 || end.wall_ns == 0 || end.cpu_ns == 0 {
            return None;
        }
        Some((
            end.wall_ns.checked_sub(self.wall_ns)?,
            end.cpu_ns.checked_sub(self.cpu_ns)?,
        ))
    }
}

fn finish(stage: &'static str, object: u64, frame: u64, layers: u64, outcome: u64, start: Stamp) {
    if start.wall_ns == 0 || start.cpu_ns == 0 {
        return;
    }
    let end = Stamp {
        wall_ns: now_ns(),
        cpu_ns: clock_ns(clock::CLOCK_THREAD_CPUTIME_ID),
    };
    if let Some((wall_ns, cpu_ns)) = start.elapsed(end) {
        event(
            stage,
            object,
            frame,
            0,
            &[
                ("start_ns", start.wall_ns),
                ("wall_ns", wall_ns),
                ("cpu_ns", cpu_ns),
                ("layers", layers),
                ("outcome", outcome),
            ],
        );
    }
}

/// Sampled stage timer. Dropping records outcome zero; `finish(1)` marks success.
/// Formatting and log I/O run on the bounded asynchronous trace writer.
pub struct Span {
    stage: &'static str,
    object: u64,
    frame: u64,
    layers: u64,
    outcome: u64,
    start: Stamp,
    // Thread CPU clocks cannot be compared after moving a span to another thread.
    same_thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Span {
    /// Start a timer without reading clocks when tracing is disabled or unsampled.
    pub fn new(stage: &'static str, object: u64, frame: u64, layers: u64) -> Self {
        Self {
            stage,
            object,
            frame,
            layers,
            outcome: 0,
            start: Stamp::start(frame),
            same_thread: std::marker::PhantomData,
        }
    }
    /// Finish with a numeric result (zero is failure, one is success).
    pub fn finish(mut self, outcome: u64) {
        self.outcome = outcome;
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        finish(
            self.stage,
            self.object,
            self.frame,
            self.layers,
            self.outcome,
            self.start,
        );
    }
}

/// C ABI clock entry point used by SurfaceFlinger's RAII adapter.
#[unsafe(no_mangle)]
pub extern "C" fn droidloom_timing_start(frame: u64) -> Stamp {
    Stamp::start(frame)
}

/// C ABI stage completion. Stage IDs are fixed in DroidloomTiming.h.
#[unsafe(no_mangle)]
pub extern "C" fn droidloom_timing_finish(
    stage: u32,
    object: u64,
    frame: u64,
    layers: u64,
    outcome: u64,
    start: Stamp,
) {
    let stage = match stage {
        0 => "timing_sf_composite",
        1 => "timing_sf_task_scan",
        2 => "timing_sf_snapshot_copy",
        3 => "timing_sf_direct_prepare",
        4 => "timing_sf_direct_publish",
        5 => "timing_sf_fallback_render",
        6 => "timing_sf_bootstrap_present",
        7 => "timing_sf_bootstrap_requests",
        8 => "timing_sf_bootstrap_state",
        9 => "timing_sf_bootstrap_begin",
        10 => "timing_sf_bootstrap_prepare",
        11 => "timing_sf_bootstrap_finish",
        12 => "timing_sf_bootstrap_dequeue",
        13 => "timing_sf_bootstrap_queue",
        14 => "timing_sf_bootstrap_hwc_present",
        15 => "timing_sf_bootstrap_complete",
        16 => "timing_sf_bootstrap_release",
        17 => "timing_sf_bootstrap_cache",
        _ => return,
    };
    finish(stage, object, frame, layers, outcome, start);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_backwards_clocks_are_unknown_instead_of_zero_duration() {
        let start = Stamp {
            wall_ns: 10,
            cpu_ns: 20,
        };
        assert_eq!(
            start.elapsed(Stamp {
                wall_ns: 30,
                cpu_ns: 25
            }),
            Some((20, 5))
        );
        assert_eq!(
            start.elapsed(Stamp {
                wall_ns: 9,
                cpu_ns: 25
            }),
            None
        );
        assert_eq!(
            start.elapsed(Stamp {
                wall_ns: 30,
                cpu_ns: 19
            }),
            None
        );
        assert_eq!(Stamp::default().elapsed(start), None);
    }
    #[test]
    fn monotonic_and_thread_cpu_clocks_are_available() {
        assert!(now_ns() > 0);
        assert!(clock_ns(clock::CLOCK_THREAD_CPUTIME_ID) > 0);
        assert_eq!(std::mem::size_of::<Stamp>(), 16);
    }

    #[test]
    fn capture_child() {
        let Ok(mode) = std::env::var("DROIDLOOM_TIMING_TEST_CHILD") else {
            return;
        };
        let threads = || std::fs::read_dir("/proc/self/task").unwrap().count();
        let before = threads();
        if mode == "disabled" {
            for _ in 0..1000 {
                assert_eq!(droidloom_timing_start(1).wall_ns, 0);
                Span::new("timing_fixture", 9, 1, 3).finish(1);
            }
            assert_eq!(threads(), before);
            return;
        }
        assert!(enabled());
        assert_eq!(droidloom_timing_start(2).wall_ns, 0);
        let start = droidloom_timing_start(1);
        assert!(start.wall_ns > 0 && start.cpu_ns > 0);
        for stage in 0..18 {
            droidloom_timing_finish(stage, 9, 1, 3, 1, start);
        }
        // Unknown stage IDs must not produce a mislabeled event.
        droidloom_timing_finish(18, 9, 1, 3, 1, start);
        Span::new("timing_fixture", 9, 9, 3).finish(1);
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while trace().unwrap().written.load(Ordering::Acquire) < 19 {
            assert!(
                std::time::Instant::now() < limit,
                "trace writer did not drain"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn enabled_ffi_and_rust_spans_log_and_disabled_spans_create_no_worker() {
        for mode in ["enabled", "disabled"] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "tests::capture_child", "--nocapture"])
                .env("DROIDLOOM_TIMING_TEST_CHILD", mode)
                .env_remove("DROIDLOOM_FRAME_TRACE");
            if mode == "enabled" {
                command.env("DROIDLOOM_FRAME_TRACE", "1");
            }
            let result = command.output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let output = String::from_utf8_lossy(&result.stderr);
            let events: Vec<_> = output
                .lines()
                .filter(|l| l.contains("Droidloom frame trace:"))
                .collect();
            assert_eq!(events.len(), if mode == "enabled" { 19 } else { 0 });
            if mode == "enabled" {
                assert!(events[0].contains("stage=timing_sf_composite"));
                assert!(events[7].contains("stage=timing_sf_bootstrap_requests"));
                assert!(events[8].contains("stage=timing_sf_bootstrap_state"));
                assert!(events[17].contains("stage=timing_sf_bootstrap_cache"));
                assert!(events[18].contains("stage=timing_fixture"));
                assert!(
                    events
                        .iter()
                        .all(|l| l.contains("cpu_ns=") && l.contains("outcome=1"))
                );
            }
        }
    }
}

/// Queue a sampled event without blocking; sequence gaps and dropped_total flag loss.
pub fn event(
    stage: &'static str,
    object: u64,
    frame: u64,
    buffer: u64,
    values: &[(&'static str, u64)],
) {
    if !sampled(frame) {
        return;
    }
    let Some(trace) = trace() else {
        return;
    };
    let sequence = trace.sequence.fetch_add(1, Ordering::Relaxed) + 1;
    if sequence > MAX_RECORDS {
        return;
    }
    let mut record = Record {
        stage,
        object,
        frame,
        buffer,
        observed_ns: now_ns(),
        values: [("", 0); FIELDS],
        count: values.len().min(FIELDS),
        sequence,
    };
    record.values[..record.count].copy_from_slice(&values[..record.count]);
    if trace.sender.try_send(record).is_err() {
        trace.dropped.fetch_add(1, Ordering::Relaxed);
    }
}
