//! Privileged two-phase Android task lifecycle service.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use droidloom_composer::task_control::{TaskControlRegistry, TaskRegistrationPhase};
use droidloom_composer::{DisplayId, TaskId};
use droidloom_composer_aidl::denial::DenialPresentationSink;
use droidloom_composer_aidl::service::ComposerLifecycle;
use droidloom_denial_ipc::{SeqPacket, SeqPacketListener};
use droidloom_denial_protocol::TaskObjectId;
use droidloom_task_control::{
    decode_request, encode_response, ControlErrorCode, ControlRequest, ControlResponse,
    PROTOCOL_MAJOR, PROTOCOL_MINOR,
};

const AID_ROOT: u32 = 0;
const AID_SYSTEM: u32 = 1_000;
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(5);

type ControlFailure = (ControlErrorCode, String);

#[derive(Debug)]
struct State {
    registry: TaskControlRegistry,
    retired: BTreeSet<TaskObjectId>,
    input_scales: BTreeMap<TaskObjectId, (u32, u32)>,
    direct_tasks: BTreeMap<TaskId, TaskObjectId>,
    direct_android_displays: BTreeMap<TaskObjectId, u32>,
}

/// Cloneable coordinator shared by the control listener and Denial event pump.
#[derive(Clone)]
pub struct TaskCoordinator {
    sink: DenialPresentationSink,
    lifecycle: ComposerLifecycle,
    state: Arc<(Mutex<State>, Condvar)>,
    operation_gate: Arc<Mutex<()>>,
    direct_registration_gate: Arc<Mutex<()>>,
}

impl TaskCoordinator {
    /// Create an empty coordinator for one Composer/Denial connection.
    pub fn new(sink: DenialPresentationSink, lifecycle: ComposerLifecycle) -> Self {
        let first_object = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_nanos()).ok())
            .filter(|object| *object <= i64::MAX as u64)
            .unwrap_or(1);
        Self {
            sink,
            lifecycle,
            state: Arc::new((
                Mutex::new(State {
                    registry: TaskControlRegistry::with_first_ids(1, first_object)
                        .expect("wall-clock object seed is a positive signed 64-bit value"),
                    retired: BTreeSet::new(),
                    input_scales: BTreeMap::new(),
                    direct_tasks: BTreeMap::new(),
                    direct_android_displays: BTreeMap::new(),
                }),
                Condvar::new(),
            )),
            operation_gate: Arc::new(Mutex::new(())),
            direct_registration_gate: Arc::new(Mutex::new(())),
        }
    }

    /// Create the configured default display SurfaceFlinger requires before
    /// Android's activity services are available. This display is deliberately
    /// left unbound: later application launches still reserve independent
    /// display/window identities through the task-control protocol.
    pub fn stage_bootstrap_display(
        &self,
        package: String,
    ) -> Result<(TaskObjectId, DisplayId), String> {
        match self.reserve(0, package).map_err(|(code, message)| {
            format!("bootstrap display reservation failed ({code:?}): {message}")
        })? {
            ControlResponse::Reserved {
                object, display, ..
            } => Ok((TaskObjectId(object), DisplayId(display))),
            _ => Err("bootstrap display reservation returned an unexpected response".to_owned()),
        }
    }

    /// Mark the first configured display as launchable and wake its pending
    /// reserve request. Later reconfigures leave the bound phase unchanged.
    pub fn display_connected(&self, object: TaskObjectId) -> Result<(), String> {
        let (state, changed) = &*self.state;
        let mut state = state
            .lock()
            .map_err(|_| "task-control state lock is poisoned".to_owned())?;
        let phase = state
            .registry
            .get(object)
            .map_err(|error| error.to_string())?
            .phase;
        if phase == TaskRegistrationPhase::Staged {
            state
                .registry
                .mark_connected(object)
                .map_err(|error| error.to_string())?;
            changed.notify_all();
        }
        Ok(())
    }

    /// Record the logical-to-buffer scale carried by the latest configure.
    pub fn set_input_scale(
        &self,
        object: TaskObjectId,
        numerator: u32,
        denominator: u32,
    ) -> Result<(), String> {
        if numerator == 0 || denominator == 0 {
            return Err(format!(
                "Droidloom object {} received an invalid input scale {numerator}/{denominator}",
                object.0
            ));
        }
        let mut state = self
            .state
            .0
            .lock()
            .map_err(|_| "task-control state lock is poisoned".to_owned())?;
        state
            .registry
            .get(object)
            .map_err(|error| error.to_string())?;
        state.input_scales.insert(object, (numerator, denominator));
        Ok(())
    }

    /// Return whether an object was intentionally removed, allowing the event
    /// pump to discard an already-queued final event without accepting random
    /// unknown object identities.
    pub fn is_retired(&self, object: TaskObjectId) -> bool {
        self.state
            .0
            .lock()
            .map(|state| state.retired.contains(&object))
            .unwrap_or(false)
    }

    /// Return whether this object is a SurfaceFlinger-owned task channel and
    /// therefore must never be advertised as an Android logical display.
    pub fn is_direct_task(&self, object: TaskObjectId) -> bool {
        self.state
            .0
            .lock()
            .map(|state| state.direct_android_displays.contains_key(&object))
            .unwrap_or(false)
    }

    /// Register one launcher- or observer-verified WindowManager task for direct export.
    /// The channel reuses safe Composer window state but intentionally skips
    /// Composer hotplug: it is not an Android display. This explicit registry
    /// is also the allowlist that excludes HOME and system organizer tasks.
    pub fn register_direct_task(
        &self,
        task: TaskId,
        package: String,
        android_display: u32,
    ) -> Result<(TaskObjectId, DisplayId), String> {
        // Explicit host launches and the Android task observer can discover
        // the same task together. Wait for its complete binding before reuse.
        let _registration = self.direct_registration_gate.lock()
            .map_err(|_| "direct registration lock is poisoned".to_owned())?;
        if task.0 == 0 || task.0 > i32::MAX as u64 {
            return Err(format!("invalid Android task identity {}", task.0));
        }
        {
            // Keep one guard for both lookups. An if-let scrutinee guard
            // lives through its body; locking again there deadlocks on reopen.
            let state = self
                .state
                .0
                .lock()
                .map_err(|_| "task-control state lock is poisoned".to_owned())?;
            if let Some(existing) = state.direct_tasks.get(&task).copied() {
                let registration = state
                    .registry
                    .get(existing)
                    .map_err(|error| error.to_string())?;
                if registration.reservation.package != package
                    || registration.android_display != Some(android_display)
                {
                    return Err(format!(
                        "Android task {} is already registered with different ownership",
                        task.0
                    ));
                }
                return Ok((existing, registration.reservation.display));
            }
        }

        let response = self
            .reserve_with_kind(0, package, Some((task, android_display)))
            .map_err(|(code, message)| {
                format!("direct task reserve failed ({code:?}): {message}")
            })?;
        let ControlResponse::Reserved {
            object, display, ..
        } = response
        else {
            return Err("direct task reservation returned an unexpected response".to_owned());
        };
        let object = TaskObjectId(object);
        let display = DisplayId(display);

        let _operation = self
            .operation_gate
            .lock()
            .map_err(|_| "task-control operation lock is poisoned".to_owned())?;
        self.sink
            .bind_task(display, task)
            .map_err(|error| format!("bind direct Denial task identity: {error}"))?;
        self.lifecycle
            .bind_direct_task(display, task)
            .map_err(|error| format!("bind direct Composer task identity: {error:?}"))?;
        self.state
            .0
            .lock()
            .map_err(|_| "task-control state lock is poisoned".to_owned())?
            .registry
            .bind_task(object, task, android_display)
            .map_err(|error| error.to_string())?;
        Ok((object, display))
    }

    /// Resolve a previously authorized direct task without creating anything.
    /// SurfaceFlinger connections for HOME or other unsolicited tasks fail
    /// closed here and therefore never create a Denial window.
    pub fn direct_task_display(&self, task: TaskId) -> Result<DisplayId, String> {
        let state = self
            .state
            .0
            .lock()
            .map_err(|_| "task-control state lock is poisoned".to_owned())?;
        let object = state
            .direct_tasks
            .get(&task)
            .copied()
            .ok_or_else(|| format!("Android task {} is not authorized for export", task.0))?;
        let registration = state
            .registry
            .get(object)
            .map_err(|error| error.to_string())?;
        if registration.phase != TaskRegistrationPhase::Bound {
            return Err(format!("Android task {} export is not ready", task.0));
        }
        Ok(registration.reservation.display)
    }

    /// Remove exactly one direct task after SurfaceFlinger observes that
    /// WindowManager destroyed its layer subtree.
    pub fn retire_direct_task(&self, task: TaskId) -> Result<(), String> {
        let _operation = self
            .operation_gate
            .lock()
            .map_err(|_| "task-control operation lock is poisoned".to_owned())?;
        let object = self
            .state
            .0
            .lock()
            .map_err(|_| "task-control state lock is poisoned".to_owned())?
            .direct_tasks
            .get(&task)
            .copied()
            .ok_or_else(|| format!("Android task {} is not authorized for export", task.0))?;
        self.remove_internal(object)
            .map_err(|(code, message)| format!("retire direct task failed ({code:?}): {message}"))
    }

    /// Accept authenticated privileged clients forever. Each connection is
    /// independent so a configure wait cannot block teardown/status traffic.
    pub fn serve(&self, listener: SeqPacketListener) -> Result<(), String> {
        loop {
            let socket = listener
                .accept()
                .map_err(|error| format!("accept task-control client: {error}"))?;
            let credentials = socket
                .peer_credentials()
                .map_err(|error| format!("authenticate task-control client: {error}"))?;
            if credentials.uid != AID_ROOT && credentials.uid != AID_SYSTEM {
                eprintln!(
                    "Droidloom rejected task-control peer pid={} uid={} gid={}",
                    credentials.pid, credentials.uid, credentials.gid
                );
                continue;
            }
            let coordinator = self.clone();
            std::thread::Builder::new()
                .name("droidloom-task-client".to_owned())
                .spawn(move || {
                    if let Err(error) = coordinator.serve_connection(&socket) {
                        eprintln!("Droidloom task-control client closed: {error}");
                    }
                })
                .map_err(|error| format!("start task-control client thread: {error}"))?;
        }
    }

    fn serve_connection(&self, socket: &SeqPacket) -> Result<(), String> {
        let mut ready = false;
        loop {
            let record = socket
                .receive_record()
                .map_err(|error| format!("receive task-control record: {error}"))?;
            if !record.descriptors.is_empty() {
                return Err("task-control record carried forbidden descriptors".to_owned());
            }
            let request = decode_request(&record.bytes)
                .map_err(|error| format!("decode task-control record: {error}"))?;
            let response = match request {
                ControlRequest::Hello {
                    min_major,
                    max_major,
                } if !ready && min_major <= PROTOCOL_MAJOR && max_major >= PROTOCOL_MAJOR => {
                    ready = true;
                    ControlResponse::ServerHello {
                        major: PROTOCOL_MAJOR,
                        minor: PROTOCOL_MINOR,
                    }
                }
                ControlRequest::Hello { .. } => error_response(
                    0,
                    ControlErrorCode::Protocol,
                    "repeated or incompatible task-control hello".to_owned(),
                ),
                request if !ready => error_response(
                    request_id(&request),
                    ControlErrorCode::Protocol,
                    "task-control hello is required".to_owned(),
                ),
                request => {
                    let request_id = request_id(&request);
                    match self.handle(request) {
                        Ok(response) => response,
                        Err((code, message)) => error_response(request_id, code, message),
                    }
                }
            };
            let bytes = encode_response(&response)
                .map_err(|error| format!("encode task-control response: {error}"))?;
            socket
                .send_record(&bytes, &[])
                .map_err(|error| format!("send task-control response: {error}"))?;
        }
    }

    fn handle(&self, request: ControlRequest) -> Result<ControlResponse, ControlFailure> {
        match request {
            ControlRequest::Reserve {
                request_id,
                package,
            } => self.reserve(request_id, package),
            ControlRequest::Bind {
                request_id,
                object,
                task,
                android_display,
            } => self.bind(
                request_id,
                TaskObjectId(object),
                TaskId(task),
                android_display,
            ),
            ControlRequest::RegisterDirect {
                request_id,
                package,
                task,
                android_display,
            } => self.register_direct(request_id, package, TaskId(task), android_display),
            ControlRequest::Remove { request_id, object } => {
                self.remove(request_id, TaskObjectId(object))
            }
            ControlRequest::Hello { .. } => Err((
                ControlErrorCode::Protocol,
                "unexpected hello dispatch".to_owned(),
            )),
        }
    }

    fn reserve(&self, request_id: u64, package: String) -> Result<ControlResponse, ControlFailure> {
        self.reserve_with_kind(request_id, package, None)
    }

    fn register_direct(
        &self,
        request_id: u64,
        package: String,
        task: TaskId,
        android_display: u32,
    ) -> Result<ControlResponse, ControlFailure> {
        if android_display > i32::MAX as u32 {
            return Err((
                ControlErrorCode::InvalidRequest,
                format!("Android display ID {android_display} exceeds the framework range"),
            ));
        }
        let (object, display) = self
            .register_direct_task(task, package, android_display)
            .map_err(backend)?;
        self.sink.request_activation(display).map_err(|error| backend(error.to_string()))?;
        Ok(ControlResponse::Bound {
            request_id,
            object: object.0,
            task: task.0,
            display: display.0,
            android_display,
        })
    }

    fn reserve_with_kind(
        &self,
        request_id: u64,
        package: String,
        direct_task: Option<(TaskId, u32)>,
    ) -> Result<ControlResponse, ControlFailure> {
        let registration = {
            let _operation = self
                .operation_gate
                .lock()
                .map_err(|_| backend("task-control operation lock is poisoned"))?;
            let registration = {
                let mut state = self
                    .state
                    .0
                    .lock()
                    .map_err(|_| backend("task-control state lock is poisoned"))?;
                let registration = state.registry.reserve(package).map_err(control_error)?;
                if let Some((task, android_display)) = direct_task {
                    if state.direct_tasks.contains_key(&task) {
                        let _ = state.registry.cancel_reservation(registration.object);
                        return Err((
                            ControlErrorCode::Conflict,
                            format!("Android task {} already has a direct channel", task.0),
                        ));
                    }
                    state.direct_tasks.insert(task, registration.object);
                    state
                        .direct_android_displays
                        .insert(registration.object, android_display);
                }
                registration
            };

            if let Err(error) = self.lifecycle.stage_task(registration.reservation.clone()) {
                let mut state = self
                    .state
                    .0
                    .lock()
                    .map_err(|_| backend("task-control state lock is poisoned"))?;
                let _ = state.registry.cancel_reservation(registration.object);
                if let Some((task, _)) = direct_task {
                    state.direct_tasks.remove(&task);
                    state.direct_android_displays.remove(&registration.object);
                }
                return Err(backend(format!("stage Composer display: {error:?}")));
            }
            {
                let mut state = self
                    .state
                    .0
                    .lock()
                    .map_err(|_| backend("task-control state lock is poisoned"))?;
                state
                    .registry
                    .mark_staged(registration.object)
                    .map_err(control_error)?;
            }
            if let Err(error) = self
                .sink
                .register_task(registration.object, &registration.reservation)
            {
                let _ = self
                    .lifecycle
                    .remove_display(registration.reservation.display);
                let mut state = self
                    .state
                    .0
                    .lock()
                    .map_err(|_| backend("task-control state lock is poisoned"))?;
                let _ = state.registry.begin_removal(registration.object);
                let _ = state.registry.finish_removal(registration.object);
                state.retired.insert(registration.object);
                if let Some((task, _)) = direct_task {
                    state.direct_tasks.remove(&task);
                    state.direct_android_displays.remove(&registration.object);
                }
                return Err(backend(format!("stage Denial task: {error}")));
            }
            registration
        };

        let (state, changed) = &*self.state;
        let state = state
            .lock()
            .map_err(|_| backend("task-control state lock is poisoned"))?;
        let (state, _wait) = changed
            .wait_timeout_while(state, CONFIGURE_TIMEOUT, |state| {
                state
                    .registry
                    .get(registration.object)
                    .is_ok_and(|current| current.phase == TaskRegistrationPhase::Staged)
            })
            .map_err(|_| backend("task-control state lock is poisoned"))?;
        let connected = state
            .registry
            .get(registration.object)
            .is_ok_and(|current| {
                matches!(
                    current.phase,
                    TaskRegistrationPhase::Connected | TaskRegistrationPhase::Bound
                )
            });
        drop(state);
        if !connected {
            let _ = self.remove_internal(registration.object);
            return Err((
                ControlErrorCode::ConfigureTimeout,
                format!(
                    "request {request_id}: Denial did not configure object {} within {} ms",
                    registration.object.0,
                    CONFIGURE_TIMEOUT.as_millis()
                ),
            ));
        }
        Ok(ControlResponse::Reserved {
            request_id,
            object: registration.object.0,
            display: registration.reservation.display.0,
        })
    }

    fn bind(
        &self,
        request_id: u64,
        object: TaskObjectId,
        task: TaskId,
        android_display: u32,
    ) -> Result<ControlResponse, ControlFailure> {
        if android_display > i32::MAX as u32 {
            return Err((
                ControlErrorCode::InvalidRequest,
                format!("Android display ID {android_display} exceeds the framework range"),
            ));
        }
        let _operation = self
            .operation_gate
            .lock()
            .map_err(|_| backend("task-control operation lock is poisoned"))?;
        let registration = {
            let state = self
                .state
                .0
                .lock()
                .map_err(|_| backend("task-control state lock is poisoned"))?;
            let registration = state.registry.get(object).map_err(control_error)?.clone();
            if registration.phase == TaskRegistrationPhase::Bound
                && registration.task == Some(task)
                && registration.android_display == Some(android_display)
            {
                return Ok(ControlResponse::Bound {
                    request_id,
                    object: object.0,
                    task: task.0,
                    display: registration.reservation.display.0,
                    android_display,
                });
            }
            if registration.phase != TaskRegistrationPhase::Connected {
                return Err((
                    ControlErrorCode::Conflict,
                    format!("object {} is not awaiting task binding", object.0),
                ));
            }
            registration
        };
        self.sink
            .bind_task(registration.reservation.display, task)
            .map_err(|error| backend(format!("bind Denial task identity: {error}")))?;
        self.lifecycle
            .bind_task(registration.reservation.display, task)
            .map_err(|error| backend(format!("bind Composer task identity: {error:?}")))?;
        let spec = self
            .state
            .0
            .lock()
            .map_err(|_| backend("task-control state lock is poisoned"))?
            .registry
            .bind_task(object, task, android_display)
            .map_err(control_error)?;
        Ok(ControlResponse::Bound {
            request_id,
            object: object.0,
            task: task.0,
            display: spec.display.0,
            android_display,
        })
    }

    /// Resolve the Android task, display, and logical-to-buffer scale for a live object.
    pub fn input_route_for_object(
        &self,
        object: TaskObjectId,
    ) -> Result<(TaskId, u32, u32, u32), String> {
        let state = self
            .state
            .0
            .lock()
            .map_err(|_| "task-control state lock is poisoned".to_owned())?;
        let registration = state
            .registry
            .get(object)
            .map_err(|error| error.to_string())?;
        let android_display = registration.android_display.ok_or_else(|| {
            format!(
                "Droidloom object {} has not been bound to an Android logical display",
                object.0
            )
        })?;
        let task = registration.task.ok_or_else(|| {
            format!(
                "Droidloom object {} has not been bound to an Android task",
                object.0
            )
        })?;
        let (numerator, denominator) =
            state.input_scales.get(&object).copied().ok_or_else(|| {
                format!(
                    "Droidloom object {} has not received a configured input scale",
                    object.0
                )
            })?;
        Ok((task, android_display, numerator, denominator))
    }

    /// Resolve the real Android task behind an explicitly authorized direct object.
    pub fn direct_task_route_for_object(
        &self,
        object: TaskObjectId,
    ) -> Result<(TaskId, u32), String> {
        let state = self
            .state
            .0
            .lock()
            .map_err(|_| "task-control state lock is poisoned".to_owned())?;
        let android_display = state
            .direct_android_displays
            .get(&object)
            .copied()
            .ok_or_else(|| format!("Droidloom object {} is not a direct task", object.0))?;
        let task = state
            .direct_tasks
            .iter()
            .find_map(|(task, owned_object)| (*owned_object == object).then_some(*task))
            .ok_or_else(|| format!("Droidloom object {} has no Android task", object.0))?;
        Ok((task, android_display))
    }

    fn remove(
        &self,
        request_id: u64,
        object: TaskObjectId,
    ) -> Result<ControlResponse, ControlFailure> {
        let _operation = self
            .operation_gate
            .lock()
            .map_err(|_| backend("task-control operation lock is poisoned"))?;
        if self
            .state
            .0
            .lock()
            .map_err(|_| backend("task-control state lock is poisoned"))?
            .retired
            .contains(&object)
        {
            return Ok(ControlResponse::Removed {
                request_id,
                object: object.0,
            });
        }
        self.remove_internal(object)?;
        Ok(ControlResponse::Removed {
            request_id,
            object: object.0,
        })
    }

    fn remove_internal(&self, object: TaskObjectId) -> Result<(), ControlFailure> {
        let registration = {
            let mut state = self
                .state
                .0
                .lock()
                .map_err(|_| backend("task-control state lock is poisoned"))?;
            let registration = state.registry.get(object).map_err(control_error)?.clone();
            if registration.phase == TaskRegistrationPhase::Reserved {
                state
                    .registry
                    .cancel_reservation(object)
                    .map_err(control_error)?;
                state.input_scales.remove(&object);
                state.retired.insert(object);
                return Ok(());
            }
            if registration.phase != TaskRegistrationPhase::Removing {
                state
                    .registry
                    .begin_removal(object)
                    .map_err(control_error)?;
            }
            registration
        };

        self.sink
            .unregister_task(registration.reservation.display)
            .map_err(|error| backend(format!("destroy Denial task: {error}")))?;
        self.lifecycle
            .remove_display(registration.reservation.display)
            .map_err(|error| backend(format!("remove Composer display: {error:?}")))?;
        let mut state = self
            .state
            .0
            .lock()
            .map_err(|_| backend("task-control state lock is poisoned"))?;
        state
            .registry
            .finish_removal(object)
            .map_err(control_error)?;
        state.input_scales.remove(&object);
        if state.direct_android_displays.remove(&object).is_some() {
            state
                .direct_tasks
                .retain(|_, owned_object| *owned_object != object);
        }
        state.retired.insert(object);
        self.state.1.notify_all();
        Ok(())
    }
}

fn request_id(request: &ControlRequest) -> u64 {
    match request {
        ControlRequest::Hello { .. } => 0,
        ControlRequest::Reserve { request_id, .. }
        | ControlRequest::Bind { request_id, .. }
        | ControlRequest::RegisterDirect { request_id, .. }
        | ControlRequest::Remove { request_id, .. } => *request_id,
    }
}

fn control_error(error: impl std::fmt::Display) -> ControlFailure {
    let message = error.to_string();
    let code = if message.contains("no registration") || message.contains("no native") {
        ControlErrorCode::NotFound
    } else if message.contains("already") || message.contains("cannot move") {
        ControlErrorCode::Conflict
    } else {
        ControlErrorCode::InvalidRequest
    };
    (code, message)
}

fn backend(message: impl Into<String>) -> ControlFailure {
    (ControlErrorCode::Backend, message.into())
}

fn error_response(request_id: u64, code: ControlErrorCode, message: String) -> ControlResponse {
    let mut bounded = String::with_capacity(message.len().min(1_024));
    for character in message.chars() {
        if bounded.len() + character.len_utf8() > 1_024 {
            break;
        }
        bounded.push(character);
    }
    if bounded.is_empty() {
        bounded.push_str("unspecified task-control failure");
    }
    ControlResponse::Error {
        request_id,
        code,
        message: bounded,
    }
}
