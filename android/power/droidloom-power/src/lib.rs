//! Android performance hint sessions as Linux utilization floors.
//!
//! Android's hint sessions name the threads that produce a client's frames,
//! such as an application's UI thread and RenderThread, and report how long
//! each frame's work took against a target. Without a power HAL that acts on
//! them, those threads start every burst of work at whatever utilization the
//! kernel last measured. A frame that follows idle time then runs on a little
//! CPU at a low frequency, misses its deadline, and drops.
//!
//! The controller turns the reports into `sched_util_min` floors:
//!
//! - Work over its target raises the session's floor in proportion to the
//!   overrun. Work well under its target lets the floor decay slowly. Work in
//!   between holds it.
//! - CPU_LOAD hints and user interaction move the floor before work arrives.
//!   While the user interacts, and for a hold time after the last input, no
//!   floor decays below the interaction floor: a scroll's first heavy frame
//!   often follows many light ones.
//! - A session that stops reporting loses its floor. It keeps what it learned
//!   and applies it again when it next reports or is hinted.
//! - A thread shared by several sessions runs at the highest of their floors.
//!
//! Floors use the kernel's 0..=1024 capacity scale. [`Policy`] derives its
//! bounds from the CPU capacities. Mesa's gallium driver queue performs the
//! driver half of every GL frame, but no Android API can name it, so each
//! session also floors the driver queue threads of its own process.
#![deny(unsafe_op_in_unsafe_fn)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io;
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", target_os = "android"))]
mod scheduler;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use scheduler::{Scheduler, cpu_capacities};

/// Largest utilization the kernel accepts.
pub const UTIL_MAX: u32 = 1024;
/// Smallest raise for work over its target.
const MIN_RAISE: u32 = 16;
/// Largest raise for one report, so one outlier cannot pin the ceiling.
const MAX_RAISE: u32 = 256;
/// Decay for one report of work under 70% of its target.
const DECAY: u32 = 4;
/// Change for one CPU_LOAD_UP or CPU_LOAD_DOWN hint.
const HINT_STEP: u32 = 128;
/// Granularity of applied floors; decay then needs a syscall only every
/// few reports.
const QUANTUM: u32 = 16;
/// A session that reports nothing for this many target periods is inactive.
const STALE_PERIODS: u32 = 10;
const MIN_STALE: Duration = Duration::from_millis(100);
const MAX_STALE: Duration = Duration::from_secs(2);
/// Driver queues start with their GL context, after the session exists.
const RESCAN: Duration = Duration::from_secs(1);
/// Interaction hold after the last input, as Denial holds its own floors.
/// Android reports touch activity at most every 100 ms; this also covers a
/// fling that continues after the finger lifts.
const INTERACTION_HOLD: Duration = Duration::from_millis(1500);

/// Floor bounds derived from CPU capacities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Floor for upcoming work of unknown size: just above the smallest tier,
    /// so the threads leave the little CPUs.
    pub baseline: u32,
    /// Floor that user interaction seeds.
    pub interaction: u32,
    /// Highest floor. It runs the largest tier with at least two CPUs at its
    /// highest frequency. A higher floor would push both of an application's
    /// frame threads onto a single prime CPU, which then serializes them.
    pub ceiling: u32,
}

impl Policy {
    /// Derive bounds from each CPU's `cpu_capacity`.
    pub fn from_capacities(capacities: &[u32]) -> Self {
        let mut tiers: Vec<(u32, usize)> = Vec::new();
        for &capacity in capacities.iter().filter(|&&capacity| capacity > 0) {
            let capacity = capacity.min(UTIL_MAX);
            match tiers.iter_mut().find(|tier| tier.0 == capacity) {
                Some(tier) => tier.1 += 1,
                None => tiers.push((capacity, 1)),
            }
        }
        tiers.sort_unstable();
        let wide = tiers
            .iter()
            .rev()
            .find(|tier| tier.1 >= 2)
            .or(tiers.last())
            .map_or(UTIL_MAX, |tier| tier.0);
        // schedutil requests a CPU's highest frequency at 80% of its capacity.
        let ceiling = wide * 4 / 5;
        let baseline = match tiers.first() {
            Some(&(little, _)) if tiers.len() > 1 && little < ceiling => {
                (little + UTIL_MAX / 16).min(ceiling)
            }
            _ => ceiling / 2,
        };
        Self {
            baseline,
            interaction: (baseline + ceiling) / 2,
            ceiling,
        }
    }
}

/// One measured unit of a session's work.
#[derive(Clone, Copy, Debug, Default)]
pub struct Work {
    /// Wall time of the work, in nanoseconds.
    pub total_ns: i64,
    /// CPU part of the work, or 0 when the client did not measure it.
    pub cpu_ns: i64,
}

/// Session hints that change CPU demand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    /// More work is coming: raise the floor ahead of it.
    LoadUp,
    /// Less work is coming.
    LoadDown,
    /// Work of unknown size is coming: start from at least the baseline.
    LoadReset,
    /// The previous work resumes after inactivity: apply the learned floor.
    LoadResume,
    /// The next work is a one-off spike: run it at the ceiling and do not
    /// learn from its report.
    LoadSpike,
}

/// Why a floor could not be applied.
#[derive(Debug)]
pub enum ApplyError {
    /// The thread has exited.
    Gone,
    /// Any other failure, such as missing privilege.
    Other(io::Error),
}

/// Applies floors to threads.
pub trait Actuator {
    /// Set one thread's floor; 0 restores the system default.
    fn apply(&mut self, tid: i32, floor: u32) -> Result<(), ApplyError>;
    /// Mesa gallium driver queue threads of one process.
    fn driver_threads(&mut self, tgid: i32) -> Vec<i32>;
}

#[derive(Debug)]
struct Session {
    tgid: i32,
    uid: i32,
    tag: i32,
    tids: Vec<i32>,
    drivers: Vec<i32>,
    target_ns: i64,
    floor: u32,
    active_until: Option<Instant>,
    scanned_at: Option<Instant>,
    spike: bool,
    paused: bool,
    efficient: bool,
}

impl Session {
    fn contains(&self, tid: i32) -> bool {
        self.tids.contains(&tid) || self.drivers.contains(&tid)
    }

    fn threads(&self) -> impl Iterator<Item = i32> + '_ {
        self.tids.iter().chain(&self.drivers).copied()
    }

    fn activate(&mut self, now: Instant) {
        let period = Duration::from_nanos(u64::try_from(self.target_ns).unwrap_or(0));
        let stale = period
            .saturating_mul(STALE_PERIODS)
            .clamp(MIN_STALE, MAX_STALE);
        self.active_until = Some(now + stale);
    }

    fn demand(&self, ceiling: u32) -> u32 {
        if self.paused || self.efficient || self.active_until.is_none() {
            0
        } else if self.spike {
            ceiling
        } else {
            self.floor.min(ceiling)
        }
    }
}

/// Hint sessions and the floors they request.
pub struct Controller<A> {
    policy: Policy,
    actuator: A,
    sessions: BTreeMap<u64, Session>,
    applied: HashMap<i32, u32>,
    next_id: u64,
    low_power: bool,
    interacting_until: Option<Instant>,
    warned: bool,
}

impl<A: Actuator> Controller<A> {
    /// A controller with no sessions.
    pub fn new(policy: Policy, actuator: A) -> Self {
        Self {
            policy,
            actuator,
            sessions: BTreeMap::new(),
            applied: HashMap::new(),
            next_id: 1,
            low_power: false,
            interacting_until: None,
            warned: false,
        }
    }

    /// The bounds in use.
    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// Open a session. It starts active at the baseline, since a client opens
    /// one when it is about to produce work.
    pub fn create(
        &mut self,
        tgid: i32,
        uid: i32,
        tag: i32,
        tids: &[i32],
        target_ns: i64,
        now: Instant,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let held = self.held(now);
        let mut session = Session {
            tgid,
            uid,
            tag,
            tids: unique(tids),
            drivers: Vec::new(),
            target_ns,
            floor: self.policy.baseline.max(held),
            active_until: None,
            scanned_at: None,
            spike: false,
            paused: false,
            efficient: false,
        };
        session.activate(now);
        self.sessions.insert(id, session);
        self.rescan(id, now);
        self.sync_session(id);
        id
    }

    /// Close a session and release the floors only it requested.
    pub fn close(&mut self, id: u64) {
        if let Some(session) = self.sessions.remove(&id) {
            self.sync(session.threads().collect());
        }
    }

    /// Change the target work duration.
    pub fn set_target(&mut self, id: u64, target_ns: i64) {
        if let Some(session) = self.sessions.get_mut(&id).filter(|_| target_ns > 0) {
            session.target_ns = target_ns;
        }
    }

    /// Learn from completed work.
    pub fn report(&mut self, id: u64, work: &[Work], now: Instant) {
        let ceiling = self.policy.ceiling;
        let held = self.held(now);
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        if session.spike {
            session.spike = false;
        } else {
            for work in work {
                session.floor = adjust(session.floor, *work, session.target_ns, ceiling);
            }
            session.floor = session.floor.max(held);
        }
        session.activate(now);
        let rescan = session.demand(ceiling) > 0
            && session
                .scanned_at
                .is_none_or(|at| now.duration_since(at) >= RESCAN);
        if rescan {
            self.rescan(id, now);
        }
        self.sync_session(id);
    }

    /// Apply a CPU load hint.
    pub fn hint(&mut self, id: u64, hint: Hint, now: Instant) {
        let policy = self.policy;
        let held = self.held(now);
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        match hint {
            Hint::LoadUp => {
                session.floor =
                    (session.floor.max(policy.baseline) + HINT_STEP).min(policy.ceiling);
            }
            Hint::LoadDown => session.floor = session.floor.saturating_sub(HINT_STEP).max(held),
            Hint::LoadReset => session.floor = session.floor.max(policy.baseline),
            Hint::LoadResume => {}
            Hint::LoadSpike => session.spike = true,
        }
        if hint != Hint::LoadDown {
            session.activate(now);
        }
        if matches!(hint, Hint::LoadReset | Hint::LoadResume) {
            self.rescan(id, now);
        }
        self.sync_session(id);
    }

    /// Stop applying a session's floor until it resumes.
    pub fn pause(&mut self, id: u64) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.paused = true;
            self.sync_session(id);
        }
    }

    /// Apply a paused session's floor again.
    pub fn resume(&mut self, id: u64, now: Instant) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.paused = false;
            session.activate(now);
            self.sync_session(id);
        }
    }

    /// Replace a session's threads. Removed threads lose its floor.
    pub fn set_threads(&mut self, id: u64, tids: &[i32], now: Instant) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let previous: Vec<i32> = session.threads().collect();
        session.tids = unique(tids);
        self.rescan(id, now);
        let mut affected = previous;
        if let Some(session) = self.sessions.get(&id) {
            affected.extend(session.threads());
        }
        self.sync(affected);
    }

    /// Mark a session as noncritical work that should not be floored.
    pub fn set_efficient(&mut self, id: u64, efficient: bool) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.efficient = efficient;
            self.sync_session(id);
        }
    }

    /// The user is interacting: frames are about to follow. Seed every
    /// critical session with at least the interaction floor and hold it there.
    pub fn interaction(&mut self, now: Instant) {
        let interaction = self.policy.interaction;
        self.interacting_until = Some(now + INTERACTION_HOLD);
        for session in self.sessions.values_mut() {
            if !session.paused && !session.efficient {
                session.floor = session.floor.max(interaction);
                session.activate(now);
            }
        }
        self.sync_all();
    }

    /// Battery saver caps every floor at the baseline.
    pub fn set_low_power(&mut self, enabled: bool) {
        if self.low_power != enabled {
            self.low_power = enabled;
            self.sync_all();
        }
    }

    /// Deactivate sessions that stopped reporting. Returns the next instant
    /// at which a session with a floor becomes inactive.
    pub fn expire(&mut self, now: Instant) -> Option<Instant> {
        let mut expired = Vec::new();
        for session in self.sessions.values_mut() {
            if session.active_until.is_some_and(|until| until <= now) {
                session.active_until = None;
                session.spike = false;
                expired.extend(session.threads());
            }
        }
        self.sync(expired);
        self.next_deadline()
    }

    /// The next instant at which [`Self::expire`] would remove a floor.
    pub fn next_deadline(&self) -> Option<Instant> {
        let ceiling = self.ceiling();
        self.sessions
            .values()
            .filter(|session| session.demand(ceiling) > 0)
            .filter_map(|session| session.active_until)
            .min()
    }

    /// Human-readable state for `dumpsys`.
    pub fn dump(&self, now: Instant) -> String {
        let Policy {
            baseline,
            interaction,
            ceiling,
        } = self.policy;
        let mut out = format!(
            "Droidloom power: baseline {baseline}, interaction {interaction}, ceiling {ceiling}, low power {}, interaction hold {} ms\n",
            self.low_power,
            self.interacting_until
                .map_or(0, |until| until.saturating_duration_since(now).as_millis())
        );
        let _ = writeln!(out, "Sessions: {}", self.sessions.len());
        for (id, session) in &self.sessions {
            let active = session.active_until.map_or_else(
                || "inactive".to_owned(),
                |until| {
                    format!(
                        "active {} ms",
                        until.saturating_duration_since(now).as_millis()
                    )
                },
            );
            let _ = writeln!(
                out,
                "  #{id} tgid {} uid {} tag {} target {} us floor {} demand {} {active}{}{}{} threads {:?} drivers {:?}",
                session.tgid,
                session.uid,
                session.tag,
                session.target_ns / 1000,
                session.floor,
                session.demand(self.ceiling()),
                if session.paused { " paused" } else { "" },
                if session.efficient { " efficient" } else { "" },
                if session.spike { " spike" } else { "" },
                session.tids,
                session.drivers,
            );
        }
        let mut applied: Vec<_> = self.applied.iter().collect();
        applied.sort_unstable();
        let _ = writeln!(out, "Applied floors: {applied:?}");
        out
    }

    /// Lowest learned floor while an interaction hold lasts.
    fn held(&self, now: Instant) -> u32 {
        if self.interacting_until.is_some_and(|until| now < until) {
            self.policy.interaction
        } else {
            0
        }
    }

    fn ceiling(&self) -> u32 {
        if self.low_power {
            self.policy.baseline
        } else {
            self.policy.ceiling
        }
    }

    fn rescan(&mut self, id: u64, now: Instant) {
        let Some(tgid) = self.sessions.get(&id).map(|session| session.tgid) else {
            return;
        };
        let found = self.actuator.driver_threads(tgid);
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        let previous = std::mem::take(&mut session.drivers);
        session.drivers = unique(&found)
            .into_iter()
            .filter(|tid| !session.tids.contains(tid))
            .collect();
        session.scanned_at = Some(now);
        let removed = previous
            .into_iter()
            .filter(|tid| !session.drivers.contains(tid))
            .collect();
        self.sync(removed);
    }

    fn sync_session(&mut self, id: u64) {
        if let Some(session) = self.sessions.get(&id) {
            self.sync(session.threads().collect());
        }
    }

    fn sync_all(&mut self) {
        let mut tids: Vec<i32> = self.sessions.values().flat_map(Session::threads).collect();
        tids.extend(self.applied.keys());
        self.sync(tids);
    }

    fn sync(&mut self, mut tids: Vec<i32>) {
        tids.sort_unstable();
        tids.dedup();
        let ceiling = self.ceiling();
        for tid in tids {
            let demand = self
                .sessions
                .values()
                .filter(|session| session.contains(tid))
                .map(|session| session.demand(ceiling))
                .max()
                .unwrap_or(0);
            let floor = if demand == 0 {
                0
            } else {
                (demand.div_ceil(QUANTUM) * QUANTUM).min(ceiling)
            };
            if floor == self.applied.get(&tid).copied().unwrap_or(0) {
                continue;
            }
            match self.actuator.apply(tid, floor) {
                Ok(()) if floor == 0 => {
                    self.applied.remove(&tid);
                }
                Ok(()) => {
                    self.applied.insert(tid, floor);
                }
                Err(ApplyError::Gone) => self.forget(tid),
                Err(ApplyError::Other(error)) => {
                    if !self.warned {
                        self.warned = true;
                        eprintln!("Droidloom power: cannot set the floor of thread {tid}: {error}");
                    }
                }
            }
        }
    }

    fn forget(&mut self, tid: i32) {
        self.applied.remove(&tid);
        for session in self.sessions.values_mut() {
            session.tids.retain(|&other| other != tid);
            session.drivers.retain(|&other| other != tid);
        }
    }
}

fn adjust(floor: u32, work: Work, target_ns: i64, ceiling: u32) -> u32 {
    let spent = if work.cpu_ns > 0 {
        work.cpu_ns
    } else {
        work.total_ns
    };
    if spent <= 0 || target_ns <= 0 {
        return floor;
    }
    if spent > target_ns {
        // Half of the overrun's fraction of the target, in utilization units.
        let raise =
            i128::from(spent - target_ns) * i128::from(UTIL_MAX) / (2 * i128::from(target_ns));
        let raise =
            u32::try_from(raise.clamp(MIN_RAISE.into(), MAX_RAISE.into())).unwrap_or(MAX_RAISE);
        (floor + raise).min(ceiling)
    } else if i128::from(spent) * 10 < i128::from(target_ns) * 7 {
        floor.saturating_sub(DECAY)
    } else {
        floor
    }
}

fn unique(tids: &[i32]) -> Vec<i32> {
    let mut tids: Vec<i32> = tids.iter().copied().filter(|&tid| tid > 0).collect();
    tids.sort_unstable();
    tids.dedup();
    tids
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOTO: [u32; 8] = [364, 364, 364, 829, 829, 829, 829, 1024];
    const TARGET: i64 = 5_833_333;

    #[derive(Default)]
    struct Recorder {
        floors: HashMap<i32, u32>,
        calls: usize,
        gone: Vec<i32>,
        drivers: HashMap<i32, Vec<i32>>,
    }

    impl Actuator for &mut Recorder {
        fn apply(&mut self, tid: i32, floor: u32) -> Result<(), ApplyError> {
            self.calls += 1;
            if self.gone.contains(&tid) {
                return Err(ApplyError::Gone);
            }
            self.floors.insert(tid, floor);
            Ok(())
        }

        fn driver_threads(&mut self, tgid: i32) -> Vec<i32> {
            self.drivers.get(&tgid).cloned().unwrap_or_default()
        }
    }

    fn work(ns: i64) -> Work {
        Work {
            total_ns: ns,
            cpu_ns: 0,
        }
    }

    fn controller(recorder: &mut Recorder) -> Controller<&mut Recorder> {
        Controller::new(Policy::from_capacities(&MOTO), recorder)
    }

    #[test]
    fn moto_bounds_keep_frame_threads_on_the_middle_tier() {
        let policy = Policy::from_capacities(&MOTO);
        assert_eq!(policy.ceiling, 663);
        assert_eq!(policy.baseline, 428);
        assert_eq!(policy.interaction, 545);
        assert!(policy.baseline > 364 && policy.ceiling < 829);
    }

    #[test]
    fn homogeneous_bounds_scale_the_only_tier() {
        let policy = Policy::from_capacities(&[1024; 4]);
        assert_eq!(policy.ceiling, 819);
        assert_eq!(policy.baseline, 409);
        assert_eq!(Policy::from_capacities(&[]).ceiling, 819);
    }

    #[test]
    fn overruns_raise_and_fast_work_decays_slowly() {
        let mut recorder = Recorder::default();
        let now = Instant::now();
        let mut controller = controller(&mut recorder);
        let id = controller.create(10, 10_100, 2, &[10, 11], TARGET, now);
        assert_eq!(controller.sessions[&id].floor, 428);
        // A 50% overrun raises by 256, which the ceiling bounds.
        controller.report(id, &[work(TARGET * 3 / 2)], now);
        assert_eq!(controller.sessions[&id].floor, 663);
        controller.report(id, &[work(TARGET * 21 / 20)], now);
        assert_eq!(controller.sessions[&id].floor, 663);
        // In between 70% and 100% the floor holds.
        controller.report(id, &[work(TARGET * 9 / 10)], now);
        assert_eq!(controller.sessions[&id].floor, 663);
        for _ in 0..10 {
            controller.report(id, &[work(TARGET / 4)], now);
        }
        assert_eq!(controller.sessions[&id].floor, 663 - 40);
        drop(controller);
        assert_eq!(recorder.floors[&10], 624);
        assert_eq!(recorder.floors[&11], 624);
    }

    #[test]
    fn cpu_duration_wins_over_wall_time() {
        let floor = adjust(
            100,
            Work {
                total_ns: TARGET * 2,
                cpu_ns: TARGET / 2,
            },
            TARGET,
            663,
        );
        assert_eq!(floor, 96);
    }

    #[test]
    fn stale_sessions_drop_their_floor_and_resume_where_they_were() {
        let mut recorder = Recorder::default();
        let start = Instant::now();
        let mut controller = controller(&mut recorder);
        let id = controller.create(10, 10_100, 2, &[10], TARGET, start);
        controller.report(id, &[work(TARGET * 2)], start);
        let deadline = controller.next_deadline().unwrap();
        assert_eq!(deadline, start + MIN_STALE);
        assert_eq!(
            controller.expire(deadline - Duration::from_millis(1)),
            Some(deadline)
        );
        assert_eq!(controller.expire(deadline), None);
        assert!(!controller.applied.contains_key(&10));
        controller.hint(id, Hint::LoadResume, deadline);
        assert_eq!(controller.applied[&10], 663);
    }

    #[test]
    fn shared_threads_take_the_highest_floor() {
        let mut recorder = Recorder::default();
        let now = Instant::now();
        let mut controller = controller(&mut recorder);
        let first = controller.create(10, 10_100, 2, &[10, 11], TARGET, now);
        let second = controller.create(10, 10_100, 2, &[10, 11], TARGET, now);
        controller.report(first, &[work(TARGET * 2)], now);
        assert_eq!(controller.applied[&10], 663);
        controller.close(first);
        assert_eq!(controller.applied[&10], 432);
        controller.close(second);
        assert!(controller.applied.is_empty());
        drop(controller);
        assert_eq!(recorder.floors[&10], 0);
    }

    #[test]
    fn interaction_seeds_critical_sessions_only() {
        let mut recorder = Recorder::default();
        let now = Instant::now();
        let mut controller = controller(&mut recorder);
        let app = controller.create(10, 10_100, 2, &[10], TARGET, now);
        let background = controller.create(20, 10_200, 2, &[20], TARGET, now);
        controller.set_efficient(background, true);
        controller.interaction(now);
        assert_eq!(controller.sessions[&app].floor, 545);
        assert_eq!(controller.applied[&10], 560);
        assert!(!controller.applied.contains_key(&20));
    }

    #[test]
    fn interaction_hold_stops_decay_until_it_ends() {
        let mut recorder = Recorder::default();
        let start = Instant::now();
        let mut controller = controller(&mut recorder);
        let id = controller.create(10, 10_100, 2, &[10], TARGET, start);
        controller.interaction(start);
        let during = start + Duration::from_millis(1400);
        for _ in 0..50 {
            controller.report(id, &[work(TARGET / 4)], during);
        }
        controller.hint(id, Hint::LoadDown, during);
        assert_eq!(controller.sessions[&id].floor, 545);
        // A window opened by the tap starts at the held floor.
        let opened = controller.create(10, 10_100, 2, &[11], TARGET, during);
        assert_eq!(controller.sessions[&opened].floor, 545);
        let after = start + INTERACTION_HOLD;
        controller.report(id, &[work(TARGET / 4)], after);
        assert_eq!(controller.sessions[&id].floor, 541);
    }

    #[test]
    fn spike_runs_at_the_ceiling_without_teaching_the_floor() {
        let mut recorder = Recorder::default();
        let now = Instant::now();
        let mut controller = controller(&mut recorder);
        let id = controller.create(10, 10_100, 2, &[10], TARGET, now);
        controller.hint(id, Hint::LoadSpike, now);
        assert_eq!(controller.applied[&10], 663);
        controller.report(id, &[work(TARGET * 4)], now);
        assert_eq!(controller.sessions[&id].floor, 428);
        assert_eq!(controller.applied[&10], 432);
    }

    #[test]
    fn low_power_caps_floors_at_the_baseline() {
        let mut recorder = Recorder::default();
        let now = Instant::now();
        let mut controller = controller(&mut recorder);
        let id = controller.create(10, 10_100, 2, &[10], TARGET, now);
        controller.report(id, &[work(TARGET * 2)], now);
        controller.set_low_power(true);
        assert_eq!(controller.applied[&10], 428);
        controller.set_low_power(false);
        assert_eq!(controller.applied[&10], 663);
    }

    #[test]
    fn driver_queues_join_and_exited_threads_are_forgotten() {
        let mut recorder = Recorder::default();
        recorder.drivers.insert(10, vec![12, 12]);
        recorder.gone.push(11);
        let now = Instant::now();
        let mut controller = controller(&mut recorder);
        let id = controller.create(10, 10_100, 2, &[10, 11], TARGET, now);
        assert_eq!(controller.sessions[&id].tids, [10]);
        assert_eq!(controller.sessions[&id].drivers, [12]);
        assert_eq!(controller.applied[&12], 432);
        controller.set_threads(id, &[13], now);
        assert!(!controller.applied.contains_key(&10));
        assert_eq!(controller.applied[&13], 432);
        assert_eq!(controller.applied[&12], 432);
    }

    #[test]
    fn unchanged_floors_need_no_syscall() {
        let mut recorder = Recorder::default();
        let now = Instant::now();
        let mut controller = controller(&mut recorder);
        let id = controller.create(10, 10_100, 2, &[10], TARGET, now);
        for _ in 0..2 {
            controller.report(id, &[work(TARGET / 4)], now);
        }
        drop(controller);
        // 428 decays to 420, which stays in the 432 quantum.
        assert_eq!(recorder.calls, 1);
    }

    #[test]
    fn paused_sessions_release_and_resume_their_floor() {
        let mut recorder = Recorder::default();
        let now = Instant::now();
        let mut controller = controller(&mut recorder);
        let id = controller.create(10, 10_100, 2, &[10], TARGET, now);
        controller.pause(id);
        assert!(controller.applied.is_empty());
        assert_eq!(controller.next_deadline(), None);
        controller.resume(id, now);
        assert_eq!(controller.applied[&10], 432);
    }
}
