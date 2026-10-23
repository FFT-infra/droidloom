//! Android's vsync from Denial's frame grants, and when to ask for grants.
//!
//! A clock subscription on the pacesetter output forwards every grant to
//! Composer, which hands it to SurfaceFlinger as a vsync. Android then runs on
//! Denial's phase-locked grid instead of a timer of its own. The pacesetter is
//! the output that sets the bootstrap display's refresh.
//!
//! Every timeline subscription, the clock's and the tasks', is armed only while
//! Android draws. After `IDLE_NANOS` without a frame they all disarm together,
//! so idle Android wakes neither Denial, the presenter, Composer nor
//! SurfaceFlinger. They disarm together because a disarm can end Denial's
//! epoch for every subscription; with all of them idle nothing is in flight.
//! The first frame after idle is shown untargeted at once and arms them again.
use super::frame_timeline::{OutputTimeline, Subscription, Target, output_timeline, wire_grant};
use super::{
    App, Connection, Dispatch, QueueHandle, endpoint_would_block, monotonic_timestamp_nanos,
    select_surface_output, wl_output,
};

const IDLE_NANOS: u64 = 250_000_000;
const AUDIT_NANOS: u64 = 1_000_000_000;

/// User data of the clock subscription.
pub(super) struct ClockTimeline;

#[derive(Default)]
pub(super) struct Clock {
    subscription: Option<Subscription>,
    armed: bool,
    last_frame_nanos: u64,
    /// Only with the `audit` diagnostic.
    audit: Option<Audit>,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            audit: droidloom_timing::traced(droidloom_timing::Diagnostic::Audit)
                .then(Audit::default),
            ..Self::default()
        }
    }

    pub fn armed(&self) -> bool {
        self.armed
    }
}

// With the `audit` diagnostic, once a second while armed: grants forwarded, frames, frames shown untargeted
// after idle, frames late for their grant, and how long before the latch
// targeted frames arrived.
#[derive(Default)]
struct Audit {
    started_nanos: u64,
    grants: u64,
    frames: u64,
    untargeted: u64,
    late_seen: u64,
    lead_count: u64,
    lead_total_nanos: u64,
    lead_min_nanos: Option<u64>,
}

impl Audit {
    fn frame(&mut self, now: u64, target: Option<&Target>) {
        self.frames += 1;
        match target {
            Some(target) => {
                let lead = target.latch_ns.saturating_sub(now);
                self.lead_count += 1;
                self.lead_total_nanos += lead;
                self.lead_min_nanos = Some(self.lead_min_nanos.map_or(lead, |min| min.min(lead)));
            }
            None => self.untargeted += 1,
        }
    }

    fn log(&mut self, now: u64, late_total: u64, force: bool) {
        if self.started_nanos == 0 {
            self.started_nanos = now;
        }
        let elapsed = now.saturating_sub(self.started_nanos);
        if elapsed < AUDIT_NANOS && !force {
            return;
        }
        eprintln!(
            "Droidloom clock audit: interval_ms={} grants={} frames={} untargeted={} late={} lead_avg_us={} lead_min_us={}",
            elapsed / 1_000_000,
            self.grants,
            self.frames,
            self.untargeted,
            late_total.saturating_sub(self.late_seen),
            self.lead_total_nanos
                .checked_div(self.lead_count)
                .unwrap_or(0)
                / 1_000,
            self.lead_min_nanos.unwrap_or(0) / 1_000,
        );
        *self = Self {
            started_nanos: now,
            late_seen: late_total,
            ..Self::default()
        };
    }
}

impl App {
    fn late_frames(&self) -> u64 {
        self.tasks.values().map(|task| task.timeline.late).sum()
    }

    fn arm_timelines(&mut self, armed: bool) {
        self.clock.armed = armed;
        if let Some(subscription) = self.clock.subscription.as_mut() {
            subscription.arm(armed);
        }
        for task in self.tasks.values_mut() {
            task.timeline.arm(armed);
        }
    }

    /// An Android frame is being shown; `target` is its grant, if any.
    pub(super) fn android_frame(&mut self, now: u64, target: Option<&Target>) {
        self.clock.last_frame_nanos = now;
        if !self.clock.armed {
            if self.clock.audit.is_some() {
                self.clock.audit = Some(Audit {
                    started_nanos: now,
                    late_seen: self.late_frames(),
                    ..Audit::default()
                });
            }
            self.arm_timelines(true);
        }
        if let Some(audit) = self.clock.audit.as_mut() {
            audit.frame(now, target);
        }
    }

    /// Called for each received grant: disarm once Android has gone idle.
    pub(super) fn clock_idle_check(&mut self, now: u64) {
        if !self.clock.armed {
            return;
        }
        let idle = now.saturating_sub(self.clock.last_frame_nanos) >= IDLE_NANOS;
        if self.clock.audit.is_some() {
            let late = self.late_frames();
            if let Some(audit) = self.clock.audit.as_mut() {
                audit.log(now, late, idle);
            }
        }
        if idle {
            self.arm_timelines(false);
        }
    }

    /// The output whose refresh the bootstrap display takes: the fastest
    /// output showing an Android task.
    fn pacesetter_output(&self) -> Option<wl_output::WlOutput> {
        self.tasks
            .values()
            .filter(|task| !task.headless() && !task.closing)
            .filter_map(|task| {
                select_surface_output(
                    &task.entered_outputs,
                    task.presentation_output.as_ref(),
                    |output| self.output_refresh_millihz(output),
                )
            })
            .max_by_key(|output| self.output_refresh_millihz(output))
            .cloned()
    }

    /// Keep the clock subscription on the pacesetter while Composer takes
    /// its vsync from the host.
    pub(super) fn refresh_clock(&mut self, qh: &QueueHandle<Self>) {
        let host_clock = self
            .endpoint
            .as_ref()
            .is_some_and(|endpoint| endpoint.host_clock());
        let output = if host_clock {
            self.pacesetter_output()
        } else {
            None
        };
        if self.clock.subscription.as_ref().map(|s| &s.output) == output.as_ref() {
            return;
        }
        self.clock.subscription = None;
        if let (Some(manager), Some(output)) = (&self.frame_timeline_manager, output) {
            let proxy = manager.get_timeline(&output, qh, ClockTimeline);
            self.clock.subscription = Some(Subscription::new(output, proxy, self.clock.armed));
        }
    }
}

impl Dispatch<OutputTimeline, ClockTimeline> for App {
    fn event(
        state: &mut Self,
        proxy: &OutputTimeline,
        event: output_timeline::Event,
        _: &ClockTimeline,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !matches!(event, output_timeline::Event::Grant { .. })
            || !state
                .clock
                .subscription
                .as_ref()
                .is_some_and(|s| s.is(proxy))
        {
            return;
        }
        let result = (|| {
            let grant = wire_grant(event)?;
            if let Some(endpoint) = state.endpoint.as_ref() {
                // A grant Composer cannot take now is worthless later.
                match endpoint.send_frame_grant(
                    grant.latch_deadline_ns,
                    grant.presentation_target_ns,
                    grant.refresh_period_ns,
                ) {
                    Err(error) if !endpoint_would_block(&error) => return Err(error.into()),
                    _ => {}
                }
            }
            if let Some(audit) = state.clock.audit.as_mut() {
                audit.grants += 1;
            }
            state.clock_idle_check(monotonic_timestamp_nanos()?);
            Ok::<(), super::PresenterError>(())
        })();
        if let Err(error) = result {
            state.fail(&error);
        }
    }
}
