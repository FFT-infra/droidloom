//! Platform-independent core of the initial Composer AIDL implementation.
//!
//! The Android Binder adapter translates frozen HWC3 commands into this state
//! machine. Droidloom requests DEVICE composition for supported unprotected
//! layers, composes them into one Denial-owned task target, and presents only
//! that completed target to the host.
//! DMA-BUF descriptors and fences remain owned by the thin platform adapter;
//! this safe core enforces their state and release-before-reuse rules.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use droidloom_transport::{
    AcceptedFrame, BufferMetadata, Configure, Damage, FormatModifier, FrameId, PresentationState,
    ReleasedFrame, TransportError,
};
use thiserror::Error;

pub mod denial;
pub mod hwc3;
pub mod task_control;

/// Stable physical display identity exported to Surface Flinger.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DisplayId(pub u64);

/// Surface Flinger layer identity.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LayerId(pub u64);

/// Android window-manager task identity represented by one native toplevel.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TaskId(pub u64);

/// Maximum number of independently hosted Android task windows in one cell.
pub const MAX_TASK_WINDOWS: usize = 4096;

/// Immutable mapping from one Android task to one host-managed display/window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskWindowSpec {
    /// Android task.
    pub task: TaskId,
    /// Android logical display dedicated to this task window.
    pub display: DisplayId,
    /// Owning Android package, reconciled by the session service.
    pub package: String,
}

/// Host reservation for one native Android window before `ActivityTaskManager`
/// has assigned the launched activity its real task identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservedTaskWindowSpec {
    /// Host-managed logical display dedicated to this future Android task.
    pub display: DisplayId,
    /// Package requested by the privileged launch controller.
    pub package: String,
}

/// Composition type requested by Surface Flinger before validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Composition {
    /// Surface Flinger composites this layer into the client target.
    Client,
    /// Composer would scan out or composite this layer directly.
    Device,
    /// Solid-color hardware layer.
    SolidColor,
    /// Cursor plane.
    Cursor,
    /// Sideband video stream.
    Sideband,
}

/// Bounded state for one Surface Flinger layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayerState {
    /// Layer identity.
    pub id: LayerId,
    /// Composition type requested before validation.
    pub requested_composition: Composition,
    /// Z order retained for future delegated-layer composition.
    pub z: i32,
    /// Protected buffers are rejected until an end-to-end protected path exists.
    pub protected_content: bool,
}

/// One composition correction returned by validateDisplay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompositionChange {
    /// Layer to update.
    pub layer: LayerId,
    /// Required composition for the initial implementation.
    pub composition: Composition,
}

/// Result of validating the current display generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationResult {
    /// Changes Surface Flinger must accept before present.
    pub composition_changes: Vec<CompositionChange>,
}

/// One client target supplied after successful validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientTarget {
    /// DMA-BUF metadata; descriptors stay in the Android wire adapter.
    pub buffer: BufferMetadata,
    /// Damage in buffer coordinates.
    pub damage: Vec<Damage>,
    /// True only when the adapter owns a valid acquire sync-file.
    pub has_acquire_fence: bool,
}

/// Successful client-target submission to the Denial-native adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentResult {
    /// Accepted frame and buffer identity.
    pub frame: AcceptedFrame,
    /// The adapter must return a present fence to Surface Flinger.
    pub requires_present_fence: bool,
}

/// Initial display power modes supported by the correctness path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerMode {
    /// Stop presentation and vertical-sync callbacks.
    Off,
    /// Normal interactive presentation.
    On,
    /// Android doze is intentionally unsupported until host power integration.
    Doze,
    /// Android doze-suspend is intentionally unsupported.
    DozeSuspend,
}

/// One callback sample derived from Denial presentation timing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VsyncSample {
    /// Display receiving the callback.
    pub display: DisplayId,
    /// Monotonic presentation timestamp.
    pub timestamp_nanos: u64,
    /// Configured period derived from millihertz.
    pub period_nanos: u64,
}

/// Invalid HWC3 command ordering or unsupported content.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ComposerError {
    /// The command addressed another display.
    #[error("display {actual:?} is unknown; this composer owns {expected:?}")]
    UnknownDisplay {
        /// Configured display.
        expected: DisplayId,
        /// Command display.
        actual: DisplayId,
    },
    /// A layer ID was reused without destroy.
    #[error("layer {0:?} already exists")]
    DuplicateLayer(LayerId),
    /// A layer command named an unknown layer.
    #[error("layer {0:?} does not exist")]
    UnknownLayer(LayerId),
    /// Display state changed after its last validation.
    #[error("display must be validated after its latest state change")]
    NotValidated,
    /// Surface Flinger has not accepted required composition changes.
    #[error("required composition changes have not been accepted")]
    ChangesNotAccepted,
    /// No client target was supplied for this present.
    #[error("present requires one client target")]
    MissingClientTarget,
    /// Present or vertical-sync enable was requested while the display was off.
    #[error("display is powered off")]
    DisplayOff,
    /// Version one deliberately omits Android-controlled doze.
    #[error("power mode {0:?} is unsupported")]
    UnsupportedPowerMode(PowerMode),
    /// A presentation timestamp moved backwards.
    #[error("presentation timestamp {actual} precedes {previous}")]
    NonMonotonicPresentation {
        /// Last accepted timestamp.
        previous: u64,
        /// Rejected timestamp.
        actual: u64,
    },
    /// Denial did not provide a usable refresh period.
    #[error("display configure has no refresh rate")]
    MissingRefreshRate,
    /// Protected content has no secure end-to-end path in version one.
    #[error("protected layer {0:?} is unsupported")]
    ProtectedContent(LayerId),
    /// DMA-BUF/configure/fence invariant failed.
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// Invalid task-to-native-window lifecycle transition.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TaskWindowError {
    /// The bounded per-cell native-window table is full.
    #[error("the Android task-window limit has been reached")]
    WindowLimit,
    /// Android task identity zero is reserved and cannot own a window.
    #[error("Android task identity must be non-zero")]
    InvalidTask,
    /// Display identity zero is reserved by the native protocol.
    #[error("Android display identity must be non-zero")]
    InvalidDisplay,
    /// The task already owns a native window.
    #[error("task {0:?} already has a native window")]
    DuplicateTask(TaskId),
    /// Another task already owns this Android logical display.
    #[error("display {0:?} already belongs to another task window")]
    DuplicateDisplay(DisplayId),
    /// The requested task has no native window.
    #[error("task {0:?} has no native window")]
    UnknownTask(TaskId),
    /// The requested display has no native task window.
    #[error("display {0:?} has no native task window")]
    UnknownDisplay(DisplayId),
    /// The staged display has not yet been bound to an `ActivityTaskManager` ID.
    #[error("display {0:?} is staged but has no Android task identity yet")]
    UnboundDisplay(DisplayId),
    /// A real Android task was already bound to this staged display.
    #[error("display {0:?} is already bound to an Android task")]
    DisplayAlreadyBound(DisplayId),
    /// Package identity is malformed or exceeds the protocol bound.
    #[error("package identity is invalid")]
    InvalidPackage,
    /// Denial still owns one or more targets for the task window.
    #[error("task {task:?} still has {buffers} buffers in flight")]
    BuffersInFlight {
        /// Task that cannot yet be removed.
        task: TaskId,
        /// Outstanding client targets.
        buffers: usize,
    },
    /// Denial still owns targets for a display that may not be task-bound yet.
    #[error("display {display:?} still has {buffers} buffers in flight")]
    DisplayBuffersInFlight {
        /// Staged display that cannot yet be removed.
        display: DisplayId,
        /// Outstanding client targets.
        buffers: usize,
    },
}

/// Single-display, single-client-target Composer state.
#[derive(Debug)]
pub struct Composer {
    display: DisplayId,
    layers: BTreeMap<LayerId, LayerState>,
    generation: u64,
    validated_generation: Option<u64>,
    changes_accepted: bool,
    client_target: Option<ClientTarget>,
    transport: PresentationState,
    power_mode: PowerMode,
    vsync_enabled: bool,
    refresh_millihz: u32,
    last_presentation_nanos: Option<u64>,
}

#[derive(Debug)]
struct TaskWindow {
    reservation: ReservedTaskWindowSpec,
    spec: Option<TaskWindowSpec>,
    composer: Composer,
}

/// Shared-cell collection of independent native Android task windows.
///
/// Each entry owns a separate host-managed Android logical display and one
/// Denial toplevel. The collection never exposes a combined Android desktop or
/// phone-screen window.
#[derive(Debug, Default)]
pub struct TaskWindowSet {
    windows: BTreeMap<DisplayId, TaskWindow>,
    tasks: BTreeMap<TaskId, DisplayId>,
}

impl TaskWindowSet {
    /// Create a native window for one Android task.
    ///
    /// # Errors
    ///
    /// Rejects duplicate task/display ownership or malformed package identity.
    pub fn create(&mut self, spec: TaskWindowSpec) -> Result<(), TaskWindowError> {
        spec.validate()?;
        let task = spec.task;
        let display = spec.display;
        self.reserve(ReservedTaskWindowSpec {
            display,
            package: spec.package,
        })?;
        if let Err(error) = self.bind_task(display, task) {
            self.windows.remove(&display);
            return Err(error);
        }
        Ok(())
    }

    /// Stage a dedicated display before Android creates the activity task.
    ///
    /// # Errors
    ///
    /// Rejects duplicate displays, malformed packages, and capacity overflow.
    pub fn reserve(&mut self, reservation: ReservedTaskWindowSpec) -> Result<(), TaskWindowError> {
        if self.windows.len() >= MAX_TASK_WINDOWS {
            return Err(TaskWindowError::WindowLimit);
        }
        reservation.validate()?;
        if self.windows.contains_key(&reservation.display) {
            return Err(TaskWindowError::DuplicateDisplay(reservation.display));
        }
        let display = reservation.display;
        self.windows.insert(
            display,
            TaskWindow {
                reservation,
                spec: None,
                composer: Composer::new(display),
            },
        );
        Ok(())
    }

    /// Bind the real `ActivityTaskManager` task ID after launch on the staged
    /// display. No provisional or synthetic Android task ID is ever exposed.
    ///
    /// # Errors
    ///
    /// Rejects zero/duplicate task IDs, unknown displays, or repeated binds.
    pub fn bind_task(
        &mut self,
        display: DisplayId,
        task: TaskId,
    ) -> Result<TaskWindowSpec, TaskWindowError> {
        if task.0 == 0 {
            return Err(TaskWindowError::InvalidTask);
        }
        if self.tasks.contains_key(&task) {
            return Err(TaskWindowError::DuplicateTask(task));
        }
        let window = self
            .windows
            .get_mut(&display)
            .ok_or(TaskWindowError::UnknownDisplay(display))?;
        if let Some(spec) = window.spec.as_ref() {
            return if spec.task == task {
                Ok(spec.clone())
            } else {
                Err(TaskWindowError::DisplayAlreadyBound(display))
            };
        }
        let spec = TaskWindowSpec {
            task,
            display,
            package: window.reservation.package.clone(),
        };
        window.spec = Some(spec.clone());
        self.tasks.insert(task, display);
        Ok(spec)
    }

    /// Access the per-task Composer state used by the Binder adapter.
    ///
    /// # Errors
    ///
    /// Rejects a task without a native window.
    pub fn composer_mut(&mut self, task: TaskId) -> Result<&mut Composer, TaskWindowError> {
        let display = self
            .tasks
            .get(&task)
            .copied()
            .ok_or(TaskWindowError::UnknownTask(task))?;
        self.windows
            .get_mut(&display)
            .map(|window| &mut window.composer)
            .ok_or(TaskWindowError::UnknownTask(task))
    }

    /// Access Composer state by the display identity carried in HWC3 commands.
    ///
    /// # Errors
    ///
    /// Rejects a display that is not assigned to a native task window.
    pub fn composer_for_display_mut(
        &mut self,
        display: DisplayId,
    ) -> Result<&mut Composer, TaskWindowError> {
        self.windows
            .get_mut(&display)
            .map(|window| &mut window.composer)
            .ok_or(TaskWindowError::UnknownDisplay(display))
    }

    /// Access immutable Composer state by its host-managed display identity.
    ///
    /// # Errors
    ///
    /// Rejects a display that is not assigned to a native task window.
    pub fn composer_for_display(&self, display: DisplayId) -> Result<&Composer, TaskWindowError> {
        self.windows
            .get(&display)
            .map(|window| &window.composer)
            .ok_or(TaskWindowError::UnknownDisplay(display))
    }

    /// Resolve an HWC3 display to its Android task identity.
    ///
    /// # Errors
    ///
    /// Rejects a display that is not assigned to a native task window.
    pub fn task_for_display(&self, display: DisplayId) -> Result<TaskId, TaskWindowError> {
        self.windows
            .get(&display)
            .ok_or(TaskWindowError::UnknownDisplay(display))?
            .spec
            .as_ref()
            .map(|spec| spec.task)
            .ok_or(TaskWindowError::UnboundDisplay(display))
    }

    /// Borrow the display/package reservation, including before task binding.
    ///
    /// # Errors
    ///
    /// Rejects a display that has no staged native window.
    pub fn reservation_for_display(
        &self,
        display: DisplayId,
    ) -> Result<&ReservedTaskWindowSpec, TaskWindowError> {
        self.windows
            .get(&display)
            .map(|window| &window.reservation)
            .ok_or(TaskWindowError::UnknownDisplay(display))
    }

    /// Iterate the immutable task/display/package mappings in task order.
    pub fn specs(&self) -> impl Iterator<Item = &TaskWindowSpec> {
        self.windows
            .values()
            .filter_map(|window| window.spec.as_ref())
    }

    /// Borrow the immutable task/display/package mapping.
    ///
    /// # Errors
    ///
    /// Rejects a task without a native window.
    pub fn get(&self, task: TaskId) -> Result<&TaskWindowSpec, TaskWindowError> {
        let display = self
            .tasks
            .get(&task)
            .copied()
            .ok_or(TaskWindowError::UnknownTask(task))?;
        self.windows
            .get(&display)
            .and_then(|window| window.spec.as_ref())
            .ok_or(TaskWindowError::UnknownTask(task))
    }

    /// Remove a task window after every Denial-owned target is released.
    ///
    /// # Errors
    ///
    /// Rejects unknown tasks and release-before-reuse violations.
    pub fn remove(&mut self, task: TaskId) -> Result<TaskWindowSpec, TaskWindowError> {
        let display = self
            .tasks
            .get(&task)
            .copied()
            .ok_or(TaskWindowError::UnknownTask(task))?;
        let buffers = self
            .windows
            .get(&display)
            .ok_or(TaskWindowError::UnknownTask(task))?
            .composer
            .in_flight_count();
        if buffers != 0 {
            return Err(TaskWindowError::BuffersInFlight { task, buffers });
        }
        self.remove_display(display)?
            .ok_or(TaskWindowError::UnknownTask(task))
    }

    /// Remove a staged display whether or not Android assigned a task yet.
    ///
    /// # Errors
    ///
    /// Rejects unknown displays and release-before-reuse violations.
    pub fn remove_display(
        &mut self,
        display: DisplayId,
    ) -> Result<Option<TaskWindowSpec>, TaskWindowError> {
        let window = self
            .windows
            .get(&display)
            .ok_or(TaskWindowError::UnknownDisplay(display))?;
        let buffers = window.composer.in_flight_count();
        if buffers != 0 {
            return Err(TaskWindowError::DisplayBuffersInFlight { display, buffers });
        }
        let window = self
            .windows
            .remove(&display)
            .ok_or(TaskWindowError::UnknownDisplay(display))?;
        if let Some(spec) = window.spec.as_ref() {
            self.tasks.remove(&spec.task);
        }
        Ok(window.spec)
    }

    /// Number of independent Android task windows.
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Whether there are no Android task windows.
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}

impl ReservedTaskWindowSpec {
    /// Validate a pre-launch display/package reservation.
    ///
    /// # Errors
    ///
    /// Rejects zero display IDs and malformed package names.
    pub fn validate(&self) -> Result<(), TaskWindowError> {
        if self.display.0 == 0 {
            return Err(TaskWindowError::InvalidDisplay);
        }
        if !valid_package(&self.package) {
            return Err(TaskWindowError::InvalidPackage);
        }
        Ok(())
    }
}

impl TaskWindowSpec {
    /// Validate identities before either `SurfaceFlinger` or Denial can observe
    /// this task mapping.
    ///
    /// # Errors
    ///
    /// Rejects reserved zero identities and malformed package names.
    pub fn validate(&self) -> Result<(), TaskWindowError> {
        if self.task.0 == 0 {
            return Err(TaskWindowError::InvalidTask);
        }
        ReservedTaskWindowSpec {
            display: self.display,
            package: self.package.clone(),
        }
        .validate()
    }
}

fn valid_package(package: &str) -> bool {
    !package.is_empty()
        && package.len() <= 255
        && package.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
}

impl Composer {
    /// Construct one physical display.
    pub fn new(display: DisplayId) -> Self {
        Self {
            display,
            layers: BTreeMap::new(),
            generation: 0,
            validated_generation: None,
            changes_accepted: false,
            client_target: None,
            transport: PresentationState::default(),
            power_mode: PowerMode::Off,
            vsync_enabled: false,
            refresh_millihz: 0,
            last_presentation_nanos: None,
        }
    }

    /// Replace format/modifier feedback received from Denial.
    pub fn replace_feedback(&mut self, formats: impl IntoIterator<Item = FormatModifier>) {
        self.transport.replace_feedback(formats);
    }

    /// Record the latest Denial configure.
    pub fn configure(&mut self, configure: Configure) {
        self.refresh_millihz = configure.refresh_millihz;
        self.transport.configure(configure);
    }

    /// Return the latest host-authoritative size and refresh configuration.
    pub fn latest_configure(&self) -> Option<Configure> {
        self.transport.latest_configure()
    }

    /// Apply Android display power without transferring host power authority.
    ///
    /// # Errors
    ///
    /// Doze modes fail until a separately reviewed Denial power bridge exists.
    pub fn set_power_mode(
        &mut self,
        display: DisplayId,
        mode: PowerMode,
    ) -> Result<(), ComposerError> {
        self.require_display(display)?;
        match mode {
            PowerMode::Off => {
                self.power_mode = mode;
                self.vsync_enabled = false;
                Ok(())
            }
            PowerMode::On => {
                self.power_mode = mode;
                Ok(())
            }
            PowerMode::Doze | PowerMode::DozeSuspend => {
                Err(ComposerError::UnsupportedPowerMode(mode))
            }
        }
    }

    /// Enable or disable vertical-sync callback delivery for the physical display.
    ///
    /// # Errors
    ///
    /// Enabling while powered off is rejected.
    pub fn set_vsync_enabled(
        &mut self,
        display: DisplayId,
        enabled: bool,
    ) -> Result<(), ComposerError> {
        self.require_display(display)?;
        if enabled && self.power_mode != PowerMode::On {
            return Err(ComposerError::DisplayOff);
        }
        self.vsync_enabled = enabled;
        Ok(())
    }

    /// Translate one Denial presentation timestamp into an optional callback.
    ///
    /// # Errors
    ///
    /// Rejects timestamps that move backwards and an enabled stream with no
    /// refresh rate from the latest configure. Multiple commits may be
    /// co-presented on one host refresh; an equal timestamp is therefore a
    /// valid duplicate sample and produces no additional callback.
    pub fn presentation_sample(
        &mut self,
        timestamp_nanos: u64,
    ) -> Result<Option<VsyncSample>, ComposerError> {
        if let Some(previous) = self.last_presentation_nanos {
            if timestamp_nanos < previous {
                return Err(ComposerError::NonMonotonicPresentation {
                    previous,
                    actual: timestamp_nanos,
                });
            }
            if timestamp_nanos == previous {
                return Ok(None);
            }
        }
        self.last_presentation_nanos = Some(timestamp_nanos);
        if self.power_mode != PowerMode::On || !self.vsync_enabled {
            return Ok(None);
        }
        if self.refresh_millihz == 0 {
            return Err(ComposerError::MissingRefreshRate);
        }
        Ok(Some(VsyncSample {
            display: self.display,
            timestamp_nanos,
            period_nanos: 1_000_000_000_000_u64 / u64::from(self.refresh_millihz),
        }))
    }

    /// Acknowledge exactly the latest Denial configure.
    ///
    /// # Errors
    ///
    /// Returns a transport error for a stale or fabricated serial.
    pub fn acknowledge_configure(&mut self, serial: u32) -> Result<(), ComposerError> {
        self.transport.acknowledge(serial)?;
        Ok(())
    }

    /// Create one Surface Flinger layer.
    ///
    /// # Errors
    ///
    /// Rejects the wrong display or a duplicate layer identity.
    pub fn create_layer(
        &mut self,
        display: DisplayId,
        layer: LayerState,
    ) -> Result<(), ComposerError> {
        self.require_display(display)?;
        if self.layers.contains_key(&layer.id) {
            return Err(ComposerError::DuplicateLayer(layer.id));
        }
        self.layers.insert(layer.id, layer);
        self.invalidate();
        Ok(())
    }

    /// Update the bounded state of an existing layer.
    ///
    /// # Errors
    ///
    /// Rejects the wrong display or an unknown layer.
    pub fn update_layer(
        &mut self,
        display: DisplayId,
        layer: LayerState,
    ) -> Result<(), ComposerError> {
        self.require_display(display)?;
        let existing = self
            .layers
            .get_mut(&layer.id)
            .ok_or(ComposerError::UnknownLayer(layer.id))?;
        if *existing != layer {
            *existing = layer;
            self.invalidate();
        }
        Ok(())
    }

    /// Read the current bounded state of one layer for an HWC3 patch command.
    ///
    /// # Errors
    ///
    /// Rejects the wrong display or an unknown layer.
    pub fn layer_state(
        &self,
        display: DisplayId,
        layer: LayerId,
    ) -> Result<LayerState, ComposerError> {
        self.require_display(display)?;
        self.layers
            .get(&layer)
            .copied()
            .ok_or(ComposerError::UnknownLayer(layer))
    }

    /// Destroy one layer.
    ///
    /// # Errors
    ///
    /// Rejects the wrong display or an unknown layer.
    pub fn destroy_layer(
        &mut self,
        display: DisplayId,
        layer: LayerId,
    ) -> Result<(), ComposerError> {
        self.require_display(display)?;
        if self.layers.remove(&layer).is_none() {
            return Err(ComposerError::UnknownLayer(layer));
        }
        self.invalidate();
        Ok(())
    }

    /// Validate the current layer generation.
    ///
    /// Supported unprotected layers are corrected to CLIENT composition so
    /// `SurfaceFlinger` produces the complete built-in-display image. Protected
    /// content fails closed.
    ///
    /// # Errors
    ///
    /// Rejects the wrong display or any protected layer.
    pub fn validate_display(
        &mut self,
        display: DisplayId,
    ) -> Result<ValidationResult, ComposerError> {
        self.require_display(display)?;
        if let Some(layer) = self.layers.values().find(|layer| layer.protected_content) {
            return Err(ComposerError::ProtectedContent(layer.id));
        }
        let composition_changes = self
            .layers
            .values()
            .filter(|layer| layer.requested_composition != Composition::Client)
            .map(|layer| CompositionChange {
                layer: layer.id,
                composition: Composition::Client,
            })
            .collect::<Vec<_>>();
        self.validated_generation = Some(self.generation);
        self.changes_accepted = composition_changes.is_empty();
        Ok(ValidationResult {
            composition_changes,
        })
    }

    /// Accept the corrections from the latest validateDisplay call.
    ///
    /// # Errors
    ///
    /// Rejects calls without a validation of the current generation.
    pub fn accept_display_changes(&mut self, display: DisplayId) -> Result<(), ComposerError> {
        self.require_current_validation(display)?;
        if self.power_mode != PowerMode::On {
            return Err(ComposerError::DisplayOff);
        }
        for layer in self.layers.values_mut() {
            layer.requested_composition = Composition::Client;
        }
        self.changes_accepted = true;
        Ok(())
    }

    /// Supply the Denial-owned target selected for Droidloom composition.
    ///
    /// The platform adapter retains descriptor ownership until present.
    ///
    /// # Errors
    ///
    /// Rejects the wrong display.
    pub fn set_client_target(
        &mut self,
        display: DisplayId,
        target: ClientTarget,
    ) -> Result<(), ComposerError> {
        self.require_display(display)?;
        self.client_target = Some(target);
        Ok(())
    }

    /// Present the validated, Droidloom-composited target to Denial.
    ///
    /// # Errors
    ///
    /// Requires current validation, accepted composition changes, one client
    /// target, latest configure acknowledgement, supported DMA-BUF feedback,
    /// and an acquire fence.
    pub fn present_display(&mut self, display: DisplayId) -> Result<PresentResult, ComposerError> {
        self.require_current_validation(display)?;
        if !self.changes_accepted {
            return Err(ComposerError::ChangesNotAccepted);
        }
        let target = self
            .client_target
            .take()
            .ok_or(ComposerError::MissingClientTarget)?;
        let frame =
            self.transport
                .submit(target.buffer, target.damage, target.has_acquire_fence)?;
        self.validated_generation = None;
        self.changes_accepted = false;
        Ok(PresentResult {
            frame,
            requires_present_fence: true,
        })
    }

    /// Complete HWC presentation after `SurfaceFlinger`'s direct render-surface
    /// bridge already submitted the final display target to Denial.
    ///
    /// # Errors
    ///
    /// Requires current validation and accepted CLIENT corrections.
    pub fn present_display_direct(&mut self, display: DisplayId) -> Result<(), ComposerError> {
        self.require_current_validation(display)?;
        if !self.changes_accepted {
            return Err(ComposerError::ChangesNotAccepted);
        }
        if self.power_mode != PowerMode::On {
            return Err(ComposerError::DisplayOff);
        }
        self.validated_generation = None;
        self.changes_accepted = false;
        Ok(())
    }

    /// Submit a complete display image rendered by `SurfaceFlinger` directly
    /// into a Denial-owned target.
    ///
    /// This bypasses HWC layer composition but retains the same configure,
    /// format, damage, in-flight-buffer, and release invariants.
    ///
    /// # Errors
    ///
    /// Rejects the wrong display, a powered-off display, or an invalid target.
    pub fn submit_rendered_target(
        &mut self,
        display: DisplayId,
        target: ClientTarget,
    ) -> Result<droidloom_transport::AcceptedFrame, ComposerError> {
        self.require_display(display)?;
        if self.power_mode != PowerMode::On {
            return Err(ComposerError::DisplayOff);
        }
        Ok(self
            .transport
            .submit(target.buffer, target.damage, target.has_acquire_fence)?)
    }

    /// Retire a presented target only after the Denial release fence exists.
    ///
    /// # Errors
    ///
    /// Rejects missing fences and unknown or duplicate frame identities.
    pub fn release_presented_target(
        &mut self,
        frame: FrameId,
        has_release_fence: bool,
    ) -> Result<ReleasedFrame, ComposerError> {
        Ok(self.transport.release(frame, has_release_fence)?)
    }

    /// Roll back a client target that failed before Denial accepted `Present`.
    ///
    /// # Errors
    ///
    /// Rejects an unknown or already retired frame.
    pub fn cancel_unsubmitted_target(
        &mut self,
        frame: FrameId,
    ) -> Result<droidloom_transport::BufferId, ComposerError> {
        Ok(self.transport.cancel_unsubmitted(frame)?)
    }

    /// Number of client targets still owned by Denial.
    pub fn in_flight_count(&self) -> usize {
        self.transport.in_flight_count()
    }

    /// Return the buffer currently owned by an in-flight frame.
    pub fn in_flight_buffer(&self, frame: FrameId) -> Option<droidloom_transport::BufferId> {
        self.transport.in_flight_buffer(frame)
    }

    fn require_display(&self, display: DisplayId) -> Result<(), ComposerError> {
        if display == self.display {
            Ok(())
        } else {
            Err(ComposerError::UnknownDisplay {
                expected: self.display,
                actual: display,
            })
        }
    }

    fn require_current_validation(&self, display: DisplayId) -> Result<(), ComposerError> {
        self.require_display(display)?;
        if self.validated_generation == Some(self.generation) {
            Ok(())
        } else {
            Err(ComposerError::NotValidated)
        }
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.validated_generation = None;
        self.changes_accepted = false;
    }
}

#[cfg(test)]
mod tests {
    use droidloom_transport::{BufferId, PlaneMetadata};

    use super::*;

    const DISPLAY: DisplayId = DisplayId(1);
    const FORMAT: FormatModifier = FormatModifier {
        fourcc: 875_713_112,
        modifier: 0,
    };

    fn composer() -> Composer {
        let mut composer = Composer::new(DISPLAY);
        composer.replace_feedback([FORMAT]);
        composer.configure(Configure {
            serial: 7,
            width: 1920,
            height: 1200,
            refresh_millihz: 60_000,
        });
        composer.acknowledge_configure(7).unwrap();
        composer.set_power_mode(DISPLAY, PowerMode::On).unwrap();
        composer
    }

    fn layer(id: u64, composition: Composition) -> LayerState {
        LayerState {
            id: LayerId(id),
            requested_composition: composition,
            z: i32::try_from(id).unwrap(),
            protected_content: false,
        }
    }

    fn target(id: u64, has_acquire_fence: bool) -> ClientTarget {
        ClientTarget {
            buffer: BufferMetadata {
                id: BufferId(id),
                width: 1920,
                height: 1200,
                format: FORMAT,
                planes: vec![PlaneMetadata {
                    index: 0,
                    offset: 0,
                    stride: 7680,
                }],
            },
            damage: vec![Damage {
                x: 0,
                y: 0,
                width: 1920,
                height: 1200,
            }],
            has_acquire_fence,
        }
    }

    #[test]
    fn layers_are_corrected_to_surfaceflinger_client_composition() {
        let mut composer = composer();
        composer
            .create_layer(DISPLAY, layer(1, Composition::Device))
            .unwrap();
        let validation = composer.validate_display(DISPLAY).unwrap();
        assert_eq!(
            validation.composition_changes,
            [CompositionChange {
                layer: LayerId(1),
                composition: Composition::Client
            }]
        );
        composer.accept_display_changes(DISPLAY).unwrap();
        composer
            .set_client_target(DISPLAY, target(10, true))
            .unwrap();
        let present = composer.present_display(DISPLAY).unwrap();
        assert!(present.requires_present_fence);
        assert_eq!(composer.in_flight_count(), 1);
        composer
            .release_presented_target(present.frame.frame_id, true)
            .unwrap();
        assert_eq!(composer.in_flight_count(), 0);
    }

    #[test]
    fn present_requires_accepted_changes_and_fresh_validation() {
        let mut composer = composer();
        composer
            .create_layer(DISPLAY, layer(1, Composition::Cursor))
            .unwrap();
        composer.validate_display(DISPLAY).unwrap();
        composer
            .set_client_target(DISPLAY, target(10, true))
            .unwrap();
        assert_eq!(
            composer.present_display(DISPLAY),
            Err(ComposerError::ChangesNotAccepted)
        );
        composer.accept_display_changes(DISPLAY).unwrap();
        let first = composer.present_display(DISPLAY).unwrap();
        composer
            .release_presented_target(first.frame.frame_id, true)
            .unwrap();
        composer
            .set_client_target(DISPLAY, target(11, true))
            .unwrap();
        assert_eq!(
            composer.present_display(DISPLAY),
            Err(ComposerError::NotValidated)
        );
    }

    #[test]
    fn protected_content_fails_closed() {
        let mut composer = composer();
        let mut protected = layer(42, Composition::Client);
        protected.protected_content = true;
        composer.create_layer(DISPLAY, protected).unwrap();
        assert_eq!(
            composer.validate_display(DISPLAY),
            Err(ComposerError::ProtectedContent(LayerId(42)))
        );
    }

    #[test]
    fn missing_acquire_and_release_fences_are_rejected() {
        let mut composer = composer();
        composer
            .create_layer(DISPLAY, layer(1, Composition::Client))
            .unwrap();
        composer.validate_display(DISPLAY).unwrap();
        composer.accept_display_changes(DISPLAY).unwrap();
        composer
            .set_client_target(DISPLAY, target(10, false))
            .unwrap();
        assert_eq!(
            composer.present_display(DISPLAY),
            Err(ComposerError::Transport(TransportError::MissingFence))
        );

        composer.validate_display(DISPLAY).unwrap();
        composer
            .set_client_target(DISPLAY, target(10, true))
            .unwrap();
        let present = composer.present_display(DISPLAY).unwrap();
        assert_eq!(
            composer.release_presented_target(present.frame.frame_id, false),
            Err(ComposerError::Transport(TransportError::MissingFence))
        );
    }

    #[test]
    fn layer_mutation_invalidates_an_earlier_validation() {
        let mut composer = composer();
        composer
            .create_layer(DISPLAY, layer(1, Composition::Client))
            .unwrap();
        composer.validate_display(DISPLAY).unwrap();
        composer
            .update_layer(DISPLAY, layer(1, Composition::SolidColor))
            .unwrap();
        composer
            .set_client_target(DISPLAY, target(10, true))
            .unwrap();
        assert_eq!(
            composer.present_display(DISPLAY),
            Err(ComposerError::NotValidated)
        );
    }

    #[test]
    fn power_and_vsync_follow_host_presentation_timing() {
        let mut composer = composer();
        composer.set_vsync_enabled(DISPLAY, true).unwrap();
        assert_eq!(
            composer.presentation_sample(1_000_000_000).unwrap(),
            Some(VsyncSample {
                display: DISPLAY,
                timestamp_nanos: 1_000_000_000,
                period_nanos: 16_666_666,
            })
        );
        assert_eq!(composer.presentation_sample(1_000_000_000).unwrap(), None);
        assert_eq!(
            composer.presentation_sample(999_999_999),
            Err(ComposerError::NonMonotonicPresentation {
                previous: 1_000_000_000,
                actual: 999_999_999,
            })
        );
        composer.set_power_mode(DISPLAY, PowerMode::Off).unwrap();
        assert_eq!(composer.presentation_sample(2_000_000_000).unwrap(), None);
        assert_eq!(
            composer.set_vsync_enabled(DISPLAY, true),
            Err(ComposerError::DisplayOff)
        );
    }

    #[test]
    fn doze_fails_until_denial_owns_the_bridge() {
        let mut composer = composer();
        assert_eq!(
            composer.set_power_mode(DISPLAY, PowerMode::Doze),
            Err(ComposerError::UnsupportedPowerMode(PowerMode::Doze))
        );
    }

    #[test]
    fn separate_tasks_own_separate_native_display_windows() {
        let mut windows = TaskWindowSet::default();
        windows
            .create(TaskWindowSpec {
                task: TaskId(10),
                display: DisplayId(100),
                package: "org.example.mail".into(),
            })
            .unwrap();
        windows
            .create(TaskWindowSpec {
                task: TaskId(11),
                display: DisplayId(101),
                package: "org.example.maps".into(),
            })
            .unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows.get(TaskId(10)).unwrap().display, DisplayId(100));
        assert_eq!(windows.get(TaskId(11)).unwrap().display, DisplayId(101));
        assert_eq!(
            windows.task_for_display(DisplayId(100)).unwrap(),
            TaskId(10)
        );
        assert_eq!(
            windows
                .specs()
                .map(|spec| (spec.task, spec.display))
                .collect::<Vec<_>>(),
            [(TaskId(10), DisplayId(100)), (TaskId(11), DisplayId(101))]
        );
        assert_eq!(
            windows.create(TaskWindowSpec {
                task: TaskId(12),
                display: DisplayId(101),
                package: "org.example.music".into(),
            }),
            Err(TaskWindowError::DuplicateDisplay(DisplayId(101)))
        );
        assert!(matches!(
            windows.composer_for_display_mut(DisplayId(999)),
            Err(TaskWindowError::UnknownDisplay(DisplayId(999)))
        ));
    }

    #[test]
    fn task_window_cannot_close_while_denial_owns_its_buffer() {
        let mut windows = TaskWindowSet::default();
        windows
            .create(TaskWindowSpec {
                task: TaskId(10),
                display: DISPLAY,
                package: "org.example.app".into(),
            })
            .unwrap();
        let composer = windows.composer_mut(TaskId(10)).unwrap();
        composer.replace_feedback([FORMAT]);
        composer.configure(Configure {
            serial: 7,
            width: 1920,
            height: 1200,
            refresh_millihz: 60_000,
        });
        composer.acknowledge_configure(7).unwrap();
        composer.set_power_mode(DISPLAY, PowerMode::On).unwrap();
        composer
            .create_layer(DISPLAY, layer(1, Composition::Client))
            .unwrap();
        composer.validate_display(DISPLAY).unwrap();
        composer.accept_display_changes(DISPLAY).unwrap();
        composer
            .set_client_target(DISPLAY, target(10, true))
            .unwrap();
        let frame = composer.present_display(DISPLAY).unwrap().frame.frame_id;

        assert_eq!(
            windows.remove(TaskId(10)),
            Err(TaskWindowError::BuffersInFlight {
                task: TaskId(10),
                buffers: 1
            })
        );
        windows
            .composer_mut(TaskId(10))
            .unwrap()
            .release_presented_target(frame, true)
            .unwrap();
        assert_eq!(
            windows.remove(TaskId(10)).unwrap().package,
            "org.example.app"
        );
        assert!(windows.is_empty());
    }

    #[test]
    fn malformed_task_identity_is_rejected() {
        let mut windows = TaskWindowSet::default();
        assert_eq!(
            windows.create(TaskWindowSpec {
                task: TaskId(0),
                display: DisplayId(1),
                package: "org.example.app".into(),
            }),
            Err(TaskWindowError::InvalidTask)
        );
        assert_eq!(
            windows.create(TaskWindowSpec {
                task: TaskId(1),
                display: DisplayId(0),
                package: "org.example.app".into(),
            }),
            Err(TaskWindowError::InvalidDisplay)
        );
        assert_eq!(
            windows.create(TaskWindowSpec {
                task: TaskId(1),
                display: DisplayId(1),
                package: "../host".into(),
            }),
            Err(TaskWindowError::InvalidPackage)
        );
    }

    #[test]
    fn task_window_table_is_bounded() {
        let mut windows = TaskWindowSet::default();
        for id in 0..MAX_TASK_WINDOWS {
            let id = u64::try_from(id).unwrap() + 1;
            windows
                .create(TaskWindowSpec {
                    task: TaskId(id),
                    display: DisplayId(id),
                    package: "org.example.app".into(),
                })
                .unwrap();
        }
        assert_eq!(
            windows.create(TaskWindowSpec {
                task: TaskId(u64::try_from(MAX_TASK_WINDOWS).unwrap()),
                display: DisplayId(u64::try_from(MAX_TASK_WINDOWS).unwrap()),
                package: "org.example.overflow".into(),
            }),
            Err(TaskWindowError::WindowLimit)
        );
    }
}
