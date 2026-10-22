//! Android Power AIDL V7 service for Droidloom.
//!
//! Android's hint sessions and interaction boosts reach this service from
//! system_server and SurfaceFlinger. `droidloom_power` turns them into
//! utilization floors on the sessions' threads. Everything that floors no
//! thread is reported unsupported, so clients do not wait for it.

#![forbid(unsafe_code)]

use std::ffi::CStr;
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use android_hardware_power::aidl::android::hardware::power::{
    Boost::Boost,
    ChannelConfig::ChannelConfig,
    CompositionData::CompositionData,
    CompositionUpdate::CompositionUpdate,
    CpuHeadroomParams::CpuHeadroomParams,
    CpuHeadroomResult::CpuHeadroomResult,
    GpuHeadroomParams::GpuHeadroomParams,
    GpuHeadroomResult::GpuHeadroomResult,
    IPower::{BnPower, IPower},
    IPowerHintSession::{BnPowerHintSession, IPowerHintSession},
    Mode::Mode,
    SessionConfig::SessionConfig,
    SessionHint::SessionHint,
    SessionMode::SessionMode,
    SessionTag::SessionTag,
    SupportInfo::SupportInfo,
    WorkDuration::WorkDuration,
};
use binder::{BinderFeatures, ExceptionCode, Interface, ProcessState, Status, Strong, ThreadState};
use droidloom_power::{Controller, Hint, Policy, Scheduler, Work, cpu_capacities};

const SERVICE_NAME: &str = "android.hardware.power.IPower/default";
/// Clients batch reports no closer together than this. HWUI reports once per
/// frame, and every frame matters.
const PREFERRED_RATE_NS: i64 = 1_000_000;
const AID_ROOT: u32 = 0;
const AID_SYSTEM: u32 = 1000;

struct State {
    controller: Controller<Scheduler>,
    /// When the expiry thread wakes next; None while it waits for work.
    wake_at: Option<Instant>,
}

struct Shared {
    state: Mutex<State>,
    expiry: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run one controller operation, then wake the expiry thread if the
    /// operation made a session's floor end earlier than it plans to wake.
    fn with<T>(&self, operation: impl FnOnce(&mut Controller<Scheduler>, Instant) -> T) -> T {
        let mut state = self.lock();
        let result = operation(&mut state.controller, Instant::now());
        if let Some(deadline) = state.controller.next_deadline()
            && state.wake_at.is_none_or(|wake_at| deadline < wake_at)
        {
            self.expiry.notify_one();
        }
        result
    }

    fn expire_forever(&self) -> ! {
        let mut state = self.lock();
        loop {
            let now = Instant::now();
            state.wake_at = state.controller.expire(now);
            state = match state.wake_at {
                Some(deadline) => {
                    let timeout = deadline.saturating_duration_since(now);
                    self.expiry
                        .wait_timeout(state, timeout)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .expiry
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}

struct Power {
    shared: Arc<Shared>,
}

/// Only Android's own services may set thread floors: hint sessions come from
/// HintManagerService and SurfaceFlinger, which have validated the threads.
fn system_caller() -> bool {
    matches!(ThreadState::get_calling_uid(), AID_ROOT | AID_SYSTEM)
}

fn unsupported<T>() -> binder::Result<T> {
    Err(Status::new_exception(
        ExceptionCode::UNSUPPORTED_OPERATION,
        None,
    ))
}

fn bit(index: i32) -> i64 {
    1 << index
}

impl Power {
    fn create(
        &self,
        tgid: i32,
        uid: i32,
        tids: &[i32],
        target_ns: i64,
        tag: SessionTag,
    ) -> binder::Result<(u64, Strong<dyn IPowerHintSession>)> {
        if !system_caller() {
            return Err(Status::new_exception(ExceptionCode::SECURITY, None));
        }
        if tids.is_empty() || target_ns <= 0 {
            return Err(Status::new_exception(ExceptionCode::ILLEGAL_ARGUMENT, None));
        }
        let id = self
            .shared
            .with(|controller, now| controller.create(tgid, uid, tag.0, tids, target_ns, now));
        let session = HintSession {
            id,
            shared: Arc::clone(&self.shared),
        };
        Ok((
            id,
            BnPowerHintSession::new_binder(session, BinderFeatures::default()),
        ))
    }
}

impl Interface for Power {
    fn dump(&self, writer: &mut dyn Write, _args: &[&CStr]) -> Result<(), binder::StatusCode> {
        let text = self.shared.lock().controller.dump(Instant::now());
        writer
            .write_all(text.as_bytes())
            .map_err(|_| binder::StatusCode::UNKNOWN_ERROR)
    }
}

impl IPower for Power {
    fn setMode(&self, mode: Mode, enabled: bool) -> binder::Result<()> {
        if mode == Mode::LOW_POWER && system_caller() {
            self.shared
                .with(|controller, _| controller.set_low_power(enabled));
        }
        Ok(())
    }

    fn isModeSupported(&self, mode: Mode) -> binder::Result<bool> {
        Ok(mode == Mode::LOW_POWER)
    }

    fn setBoost(&self, boost: Boost, _duration_ms: i32) -> binder::Result<()> {
        if boost == Boost::INTERACTION && system_caller() {
            self.shared.with(Controller::interaction);
        }
        Ok(())
    }

    fn isBoostSupported(&self, boost: Boost) -> binder::Result<bool> {
        Ok(boost == Boost::INTERACTION)
    }

    fn createHintSession(
        &self,
        tgid: i32,
        uid: i32,
        tids: &[i32],
        target_ns: i64,
    ) -> binder::Result<Strong<dyn IPowerHintSession>> {
        self.create(tgid, uid, tids, target_ns, SessionTag::OTHER)
            .map(|(_, session)| session)
    }

    fn getHintSessionPreferredRate(&self) -> binder::Result<i64> {
        Ok(PREFERRED_RATE_NS)
    }

    fn createHintSessionWithConfig(
        &self,
        tgid: i32,
        uid: i32,
        tids: &[i32],
        target_ns: i64,
        tag: SessionTag,
        config: &mut SessionConfig,
    ) -> binder::Result<Strong<dyn IPowerHintSession>> {
        let (id, session) = self.create(tgid, uid, tids, target_ns, tag)?;
        config.id = i64::try_from(id).unwrap_or(i64::MAX);
        Ok(session)
    }

    fn getSessionChannel(&self, _tgid: i32, _uid: i32) -> binder::Result<ChannelConfig> {
        // HintManagerService treats a channel without an event flag as "no
        // message queue" and keeps every client on Binder reports. An
        // exception here would instead fail each app's channel request.
        Ok(ChannelConfig::default())
    }

    fn closeSessionChannel(&self, _tgid: i32, _uid: i32) -> binder::Result<()> {
        Ok(())
    }

    fn getSupportInfo(&self) -> binder::Result<SupportInfo> {
        Ok(SupportInfo {
            usesSessions: true,
            boosts: bit(Boost::INTERACTION.0),
            modes: bit(Mode::LOW_POWER.0),
            sessionHints: [
                SessionHint::CPU_LOAD_UP,
                SessionHint::CPU_LOAD_DOWN,
                SessionHint::CPU_LOAD_RESET,
                SessionHint::CPU_LOAD_RESUME,
                SessionHint::POWER_EFFICIENCY,
                SessionHint::CPU_LOAD_SPIKE,
            ]
            .iter()
            .map(|hint| bit(hint.0))
            .sum(),
            sessionModes: bit(SessionMode::POWER_EFFICIENCY.0),
            sessionTags: [
                SessionTag::OTHER,
                SessionTag::SURFACEFLINGER,
                SessionTag::HWUI,
                SessionTag::GAME,
                SessionTag::APP,
                SessionTag::SYSUI,
            ]
            .iter()
            .map(|tag| bit(tag.0))
            .sum(),
            ..SupportInfo::default()
        })
    }

    fn getCpuHeadroom(&self, _params: &CpuHeadroomParams) -> binder::Result<CpuHeadroomResult> {
        unsupported()
    }

    fn getGpuHeadroom(&self, _params: &GpuHeadroomParams) -> binder::Result<GpuHeadroomResult> {
        unsupported()
    }

    fn sendCompositionData(&self, _data: &[CompositionData]) -> binder::Result<()> {
        Ok(())
    }

    fn sendCompositionUpdate(&self, _update: &CompositionUpdate) -> binder::Result<()> {
        Ok(())
    }
}

struct HintSession {
    id: u64,
    shared: Arc<Shared>,
}

impl Interface for HintSession {}

impl Drop for HintSession {
    // A client that dies without closing releases its binder reference.
    fn drop(&mut self) {
        let id = self.id;
        self.shared.with(|controller, _| controller.close(id));
    }
}

impl IPowerHintSession for HintSession {
    fn updateTargetWorkDuration(&self, target_ns: i64) -> binder::Result<()> {
        let id = self.id;
        self.shared
            .with(|controller, _| controller.set_target(id, target_ns));
        Ok(())
    }

    fn reportActualWorkDuration(&self, durations: &[WorkDuration]) -> binder::Result<()> {
        let id = self.id;
        let work: Vec<Work> = durations
            .iter()
            .map(|duration| Work {
                total_ns: duration.durationNanos,
                cpu_ns: duration.cpuDurationNanos,
            })
            .collect();
        self.shared
            .with(|controller, now| controller.report(id, &work, now));
        Ok(())
    }

    fn pause(&self) -> binder::Result<()> {
        let id = self.id;
        self.shared.with(|controller, _| controller.pause(id));
        Ok(())
    }

    fn resume(&self) -> binder::Result<()> {
        let id = self.id;
        self.shared
            .with(|controller, now| controller.resume(id, now));
        Ok(())
    }

    fn close(&self) -> binder::Result<()> {
        let id = self.id;
        self.shared.with(|controller, _| controller.close(id));
        Ok(())
    }

    fn sendHint(&self, hint: SessionHint) -> binder::Result<()> {
        let id = self.id;
        let hint = match hint {
            SessionHint::CPU_LOAD_UP => Hint::LoadUp,
            SessionHint::CPU_LOAD_DOWN => Hint::LoadDown,
            SessionHint::CPU_LOAD_RESET => Hint::LoadReset,
            SessionHint::CPU_LOAD_RESUME => Hint::LoadResume,
            SessionHint::CPU_LOAD_SPIKE => Hint::LoadSpike,
            SessionHint::POWER_EFFICIENCY => {
                self.shared
                    .with(|controller, _| controller.set_efficient(id, true));
                return Ok(());
            }
            _ => return Ok(()),
        };
        self.shared
            .with(|controller, now| controller.hint(id, hint, now));
        Ok(())
    }

    fn setThreads(&self, tids: &[i32]) -> binder::Result<()> {
        if tids.is_empty() {
            return Err(Status::new_exception(ExceptionCode::ILLEGAL_ARGUMENT, None));
        }
        let id = self.id;
        self.shared
            .with(|controller, now| controller.set_threads(id, tids, now));
        Ok(())
    }

    fn setMode(&self, mode: SessionMode, enabled: bool) -> binder::Result<()> {
        if mode == SessionMode::POWER_EFFICIENCY {
            let id = self.id;
            self.shared
                .with(|controller, _| controller.set_efficient(id, enabled));
        }
        Ok(())
    }

    fn getSessionConfig(&self) -> binder::Result<SessionConfig> {
        Ok(SessionConfig {
            id: i64::try_from(self.id).unwrap_or(i64::MAX),
        })
    }
}

fn main() {
    let policy = Policy::from_capacities(&cpu_capacities());
    eprintln!(
        "Droidloom power: baseline {}, interaction {}, ceiling {}",
        policy.baseline, policy.interaction, policy.ceiling
    );
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            controller: Controller::new(policy, Scheduler::new()),
            wake_at: None,
        }),
        expiry: Condvar::new(),
    });
    let expiry = Arc::clone(&shared);
    if let Err(error) = std::thread::Builder::new()
        .name("power-expiry".to_owned())
        .spawn(move || expiry.expire_forever())
    {
        eprintln!("Droidloom power fatal: start expiry thread: {error}");
        std::process::exit(1);
    }

    ProcessState::set_thread_pool_max_thread_count(2);
    ProcessState::start_thread_pool();
    let service = BnPower::new_binder(Power { shared }, BinderFeatures::default());
    if let Err(error) = binder::add_service(SERVICE_NAME, service.as_binder()) {
        eprintln!("Droidloom power fatal: register {SERVICE_NAME}: {error:?}");
        std::process::exit(1);
    }
    ProcessState::join_thread_pool();
}
