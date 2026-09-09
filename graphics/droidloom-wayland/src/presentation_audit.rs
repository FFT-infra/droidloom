//! Optional aggregate timing diagnostics. No input events or application content.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use droidloom_syncobj::frame_trace;

pub(super) struct FrameTrace {
    object: u64,
    frame: u64,
    buffer: u64,
    acquire_fence: Option<OwnedFd>,
    gpu_logged: bool,
    gpu_known: bool,
    flushed: bool,
    pub(super) release_observed: bool,
}

impl FrameTrace {
    pub(super) fn new(object: u64, frame: u64, buffer: u64, fence: BorrowedFd<'_>) -> Option<Self> {
        if !frame_trace::sampled(frame) { return None; }
        let mut trace = Self { object, frame, buffer,
            acquire_fence: fence.try_clone_to_owned().ok(), gpu_logged: false, gpu_known: false,
            flushed: false, release_observed: false };
        trace.event("native_present_begin", &[]);
        trace.gpu();
        Some(trace)
    }
    fn event(&self, stage: &'static str, fields: &[(&'static str, u64)]) {
        frame_trace::event(stage, self.object, self.frame, self.buffer, fields);
    }
    fn gpu(&mut self) {
        if self.gpu_logged { return; }
        let result = self.acquire_fence.as_ref().map(|fd| frame_trace::fence_signal_ns(fd.as_fd()));
        match result {
            Some(Ok(Some(time))) => {
                self.event("gpu_complete", &[("signal_ns", time)]);
                self.gpu_logged = true;
                self.gpu_known = true;
            }
            Some(Ok(None)) => {}
            _ => {
                self.event("gpu_timestamp_unavailable", &[]);
                self.gpu_logged = true;
            }
        }
    }
    pub(super) fn commit(&mut self) { self.event("wayland_commit", &[]); }
    pub(super) fn flushed(&mut self) {
        if !self.flushed { self.event("wayland_flushed", &[]); self.flushed = true; }
    }
    pub(super) fn release_ready(&mut self, feedback_pending: bool) {
        self.gpu();
        self.event("release_observed", &[("feedback_pending", u64::from(feedback_pending))]);
        self.release_observed = true;
    }
    pub(super) fn returned(&mut self) {
        self.gpu();
        self.event("buffer_return_sent", &[("gpu_timestamp_known", u64::from(self.gpu_known))]);
    }
}

#[derive(Default)]
pub(super) struct Audit {
    start: u64,
    presentations: u64,
    invalid_timestamps: u64,
    present_sum: u64,
    present_max: u64,
    delivery_sum: u64,
    delivery_max: u64,
    releases: u64,
    release_sum: u64,
    release_max: u64,
    pending_max: usize,
}

impl Audit {
    pub(super) fn released(&mut self, submitted: u64, now: u64) {
        let age = now.saturating_sub(submitted);
        self.releases += 1;
        self.release_sum += age;
        self.release_max = self.release_max.max(age);
    }

    pub(super) fn presented(&mut self, object: u64, submitted: u64, displayed: u64,
        now: u64, pending: usize) {
        if self.start == 0 { self.start = now; }
        if let (Some(age), Some(delivery)) = (displayed.checked_sub(submitted), now.checked_sub(displayed)) {
            self.presentations += 1;
            self.present_sum += age;
            self.present_max = self.present_max.max(age);
            self.delivery_sum += delivery;
            self.delivery_max = self.delivery_max.max(delivery);
        } else {
            self.invalid_timestamps += 1;
        }
        self.pending_max = self.pending_max.max(pending);
        if now.saturating_sub(self.start) < 1_000_000_000 { return; }
        let count = self.presentations.max(1);
        eprintln!("Droidloom presentation audit: object={object} interval_ms={} presentations={} invalid_timestamps={} commit_to_present_avg_us={} commit_to_present_max_us={} feedback_delivery_avg_us={} feedback_delivery_max_us={} releases={} commit_to_release_avg_us={} commit_to_release_max_us={} pending_max={}",
            (now - self.start) / 1_000_000, self.presentations, self.invalid_timestamps,
            self.present_sum / count / 1_000, self.present_max / 1_000,
            self.delivery_sum / count / 1_000, self.delivery_max / 1_000,
            self.releases, self.release_sum / self.releases.max(1) / 1_000,
            self.release_max / 1_000, self.pending_max);
        *self = Self { start: now, ..Self::default() };
    }
}
