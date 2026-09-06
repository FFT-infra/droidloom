//! Bounded, Binder-independent translation of frozen Composer3 command batches.
//!
//! Generated AIDL types are deliberately confined to the Android service
//! binary. That thin binary converts them into these owned values, while this
//! module enforces command bounds, display isolation, ordering, and stable
//! Composer3 service-specific error codes under normal Cargo tests.

#![forbid(unsafe_code)]

use droidloom_transport::{AcceptedFrame, FormatModifier, FrameId, ReleasedFrame};

use crate::{
    ClientTarget, ComposerError, Composition, CompositionChange, DisplayId, LayerId, LayerState,
    PowerMode, TaskWindowError, TaskWindowSet, TaskWindowSpec,
};

/// Maximum display commands accepted in one Binder transaction.
pub const MAX_DISPLAY_COMMANDS: usize = 4096;
/// Maximum layer patches accepted for one display command.
pub const MAX_LAYER_COMMANDS: usize = 4096;
/// Maximum client-target damage rectangles accepted per display command.
pub const MAX_DAMAGE_RECTS: usize = 256;
/// Maximum buffer slots accepted when Surface Flinger creates a layer.
pub const MAX_BUFFER_SLOTS: u32 = 4096;

/// Composer3 service-specific exception values frozen by `IComposerClient` V5.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ErrorCode {
    /// Display configuration is unknown.
    BadConfig = 1,
    /// Display identity is unknown.
    BadDisplay = 2,
    /// Layer identity is unknown or already used.
    BadLayer = 3,
    /// Payload shape, fence, or command combination is invalid.
    BadParameter = 4,
    /// Buffer or object resources are temporarily exhausted.
    NoResources = 6,
    /// Present was requested without a current accepted validation.
    NotValidated = 7,
    /// The requested operation has no correct initial implementation.
    Unsupported = 8,
}

/// AIDL `LayerCommand` fields consumed by the all-client composition path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayerPatch {
    /// Existing Surface Flinger layer.
    pub layer: LayerId,
    /// Optional requested composition type.
    pub composition: Option<Composition>,
    /// Optional Z order.
    pub z: Option<i32>,
    /// Whether the imported buffer is protected.
    pub protected_content: Option<bool>,
    /// True when AIDL carried a sideband native handle, which v1 rejects.
    pub has_sideband_stream: bool,
}

/// Owned, platform-neutral subset of one frozen AIDL `DisplayCommand`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "these independent flags exactly mirror frozen HWC3 DisplayCommand V5"
)]
pub struct DisplayCommand {
    /// Per-task display addressed by this command.
    pub display: DisplayId,
    /// Ordered layer state patches.
    pub layers: Vec<LayerPatch>,
    /// Optional Surface Flinger client target after native-handle import.
    pub client_target: Option<ClientTarget>,
    /// Request `validateDisplay` after applying state.
    pub validate_display: bool,
    /// Request `acceptDisplayChanges` after validation.
    pub accept_display_changes: bool,
    /// Request `presentDisplay` after preceding operations.
    pub present_display: bool,
    /// Request the combined HWC3 present-or-validate operation.
    pub present_or_validate_display: bool,
}

/// Result discriminator for HWC3 `presentOrValidateDisplay`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentOrValidateResult {
    /// The current frame was presented.
    Presented,
    /// The current state was validated and may require acceptance.
    Validated,
}

/// Platform-neutral payload converted to AIDL `CommandResultPayload`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandResult {
    /// Per-command error; later display commands remain independently usable.
    Error {
        /// Zero-based index in the input transaction.
        command_index: u32,
        /// Frozen service-specific error.
        code: ErrorCode,
    },
    /// Composition corrections returned by validation.
    ChangedCompositionTypes {
        /// Display whose layers changed.
        display: DisplayId,
        /// All required CLIENT corrections.
        layers: Vec<CompositionChange>,
    },
    /// Client target accepted by the Denial transport.
    Present {
        /// Presented task display.
        display: DisplayId,
        /// Frame forwarded to the Denial-native wire adapter.
        frame: AcceptedFrame,
        /// True when the Binder adapter must return a present fence.
        requires_present_fence: bool,
    },
    /// `SurfaceFlinger`'s direct host target was already submitted by the
    /// render-surface bridge, so HWC has no second present operation or fence.
    PresentDirect {
        /// Presented built-in display.
        display: DisplayId,
    },
    /// Outcome of the combined HWC3 operation.
    PresentOrValidate {
        /// Display receiving the operation.
        display: DisplayId,
        /// Whether the command presented or validated.
        result: PresentOrValidateResult,
    },
}

/// One Composer Binder-client session over independent task displays.
#[derive(Debug, Default)]
pub struct Session {
    windows: TaskWindowSet,
    next_layer: u64,
}

impl Session {
    /// Borrow the task-window collection for host lifecycle/configure events.
    pub fn windows(&self) -> &TaskWindowSet {
        &self.windows
    }

    /// Mutably borrow task windows for DMA-BUF feedback and configure events.
    pub fn windows_mut(&mut self) -> &mut TaskWindowSet {
        &mut self.windows
    }

    /// Add one host-managed Android task display/native Denial window.
    ///
    /// # Errors
    ///
    /// Preserves the fail-closed task/display/package checks of
    /// [`TaskWindowSet::create`].
    pub fn create_task_window(&mut self, spec: TaskWindowSpec) -> Result<(), TaskWindowError> {
        self.windows.create(spec)
    }

    /// Allocate a layer identity on exactly one task display.
    ///
    /// # Errors
    ///
    /// Returns frozen HWC3 errors for an unknown display, invalid slot count,
    /// or exhausted layer identity space.
    pub fn create_layer(
        &mut self,
        display: DisplayId,
        buffer_slot_count: u32,
    ) -> Result<LayerId, ErrorCode> {
        if buffer_slot_count == 0 || buffer_slot_count > MAX_BUFFER_SLOTS {
            return Err(ErrorCode::BadParameter);
        }
        let next = self
            .next_layer
            .checked_add(1)
            .ok_or(ErrorCode::NoResources)?;
        let layer = LayerId(next);
        let composer = self
            .windows
            .composer_for_display_mut(display)
            .map_err(|error| map_task_error(&error))?;
        composer
            .create_layer(
                display,
                LayerState {
                    id: layer,
                    requested_composition: Composition::Client,
                    z: 0,
                    protected_content: false,
                },
            )
            .map_err(|error| map_composer_error(&error))?;
        self.next_layer = next;
        Ok(layer)
    }

    /// Destroy a layer on its owning task display.
    ///
    /// # Errors
    ///
    /// Maps internal ownership failures to frozen HWC3 error values.
    pub fn destroy_layer(&mut self, display: DisplayId, layer: LayerId) -> Result<(), ErrorCode> {
        self.windows
            .composer_for_display_mut(display)
            .map_err(|error| map_task_error(&error))?
            .destroy_layer(display, layer)
            .map_err(|error| map_composer_error(&error))
    }

    /// Apply Android display power to exactly one task window.
    ///
    /// # Errors
    ///
    /// Rejects unknown displays and modes without a correct host integration.
    pub fn set_power_mode(&mut self, display: DisplayId, mode: PowerMode) -> Result<(), ErrorCode> {
        self.windows
            .composer_for_display_mut(display)
            .map_err(|error| map_task_error(&error))?
            .set_power_mode(display, mode)
            .map_err(|error| map_composer_error(&error))
    }

    /// Enable or disable callbacks for exactly one task display.
    ///
    /// # Errors
    ///
    /// Rejects unknown or powered-off displays.
    pub fn set_vsync_enabled(
        &mut self,
        display: DisplayId,
        enabled: bool,
    ) -> Result<(), ErrorCode> {
        self.windows
            .composer_for_display_mut(display)
            .map_err(|error| map_task_error(&error))?
            .set_vsync_enabled(display, enabled)
            .map_err(|error| map_composer_error(&error))
    }

    /// Execute an ordered, bounded AIDL-normalized command transaction.
    ///
    /// Invalid commands produce indexed result payloads without mutating other
    /// task displays in the batch.
    pub fn execute_commands(&mut self, commands: Vec<DisplayCommand>) -> Vec<CommandResult> {
        if commands.len() > MAX_DISPLAY_COMMANDS {
            return vec![CommandResult::Error {
                command_index: u32::try_from(MAX_DISPLAY_COMMANDS).unwrap_or(u32::MAX),
                code: ErrorCode::BadParameter,
            }];
        }
        let mut results = Vec::new();
        for (index, command) in commands.into_iter().enumerate() {
            if let Err(code) = self.execute_one(command, &mut results) {
                results.push(CommandResult::Error {
                    command_index: u32::try_from(index).unwrap_or(u32::MAX),
                    code,
                });
            }
        }
        results
    }

    /// Release a presented client target after Denial supplies its fence.
    ///
    /// # Errors
    ///
    /// Maps missing fences, stale frames, and unknown displays to HWC3 values.
    pub fn release_presented_target(
        &mut self,
        display: DisplayId,
        frame: FrameId,
        has_release_fence: bool,
    ) -> Result<ReleasedFrame, ErrorCode> {
        self.windows
            .composer_for_display_mut(display)
            .map_err(|error| map_task_error(&error))?
            .release_presented_target(frame, has_release_fence)
            .map_err(|error| map_composer_error(&error))
    }

    fn execute_one(
        &mut self,
        command: DisplayCommand,
        results: &mut Vec<CommandResult>,
    ) -> Result<(), ErrorCode> {
        if command.layers.len() > MAX_LAYER_COMMANDS
            || command
                .client_target
                .as_ref()
                .is_some_and(|target| target.damage.len() > MAX_DAMAGE_RECTS)
        {
            return Err(ErrorCode::BadParameter);
        }
        if command.present_or_validate_display
            && (command.validate_display
                || command.accept_display_changes
                || command.present_display)
        {
            return Err(ErrorCode::BadParameter);
        }

        let display = command.display;
        let composer = self
            .windows
            .composer_for_display_mut(display)
            .map_err(|error| map_task_error(&error))?;
        for patch in command.layers {
            if patch.has_sideband_stream {
                return Err(ErrorCode::Unsupported);
            }
            let mut state = composer
                .layer_state(display, patch.layer)
                .map_err(|error| map_composer_error(&error))?;
            if let Some(composition) = patch.composition {
                state.requested_composition = composition;
            }
            if let Some(z) = patch.z {
                state.z = z;
            }
            if let Some(protected) = patch.protected_content {
                state.protected_content = protected;
            }
            composer
                .update_layer(display, state)
                .map_err(|error| map_composer_error(&error))?;
        }
        if let Some(target) = command.client_target {
            composer
                .set_client_target(display, target)
                .map_err(|error| map_composer_error(&error))?;
        }

        if command.present_or_validate_display {
            return match composer.present_display_direct(display) {
                Ok(()) => {
                    results.push(CommandResult::PresentDirect { display });
                    results.push(CommandResult::PresentOrValidate {
                        display,
                        result: PresentOrValidateResult::Presented,
                    });
                    Ok(())
                }
                Err(ComposerError::NotValidated) => {
                    let validation = composer
                        .validate_display(display)
                        .map_err(|error| map_composer_error(&error))?;
                    push_changes(results, display, validation.composition_changes);
                    results.push(CommandResult::PresentOrValidate {
                        display,
                        result: PresentOrValidateResult::Validated,
                    });
                    Ok(())
                }
                Err(error) => Err(map_composer_error(&error)),
            };
        }

        if command.validate_display {
            let validation = composer
                .validate_display(display)
                .map_err(|error| map_composer_error(&error))?;
            push_changes(results, display, validation.composition_changes);
        }
        if command.accept_display_changes {
            composer
                .accept_display_changes(display)
                .map_err(|error| map_composer_error(&error))?;
        }
        if command.present_display {
            composer
                .present_display_direct(display)
                .map_err(|error| map_composer_error(&error))?;
            results.push(CommandResult::PresentDirect { display });
        }
        Ok(())
    }
}

fn push_changes(
    results: &mut Vec<CommandResult>,
    display: DisplayId,
    layers: Vec<CompositionChange>,
) {
    if !layers.is_empty() {
        results.push(CommandResult::ChangedCompositionTypes { display, layers });
    }
}

fn map_task_error(error: &TaskWindowError) -> ErrorCode {
    match error {
        TaskWindowError::UnknownDisplay(_) | TaskWindowError::UnknownTask(_) => {
            ErrorCode::BadDisplay
        }
        TaskWindowError::WindowLimit
        | TaskWindowError::BuffersInFlight { .. }
        | TaskWindowError::DisplayBuffersInFlight { .. } => ErrorCode::NoResources,
        TaskWindowError::InvalidTask
        | TaskWindowError::InvalidDisplay
        | TaskWindowError::DuplicateTask(_)
        | TaskWindowError::DuplicateDisplay(_)
        | TaskWindowError::UnboundDisplay(_)
        | TaskWindowError::DisplayAlreadyBound(_)
        | TaskWindowError::InvalidPackage => ErrorCode::BadParameter,
    }
}

fn map_composer_error(error: &ComposerError) -> ErrorCode {
    use droidloom_transport::TransportError;

    match error {
        ComposerError::UnknownDisplay { .. } => ErrorCode::BadDisplay,
        ComposerError::DuplicateLayer(_) | ComposerError::UnknownLayer(_) => ErrorCode::BadLayer,
        ComposerError::NotValidated | ComposerError::ChangesNotAccepted => ErrorCode::NotValidated,
        ComposerError::UnsupportedPowerMode(_) | ComposerError::ProtectedContent(_) => {
            ErrorCode::Unsupported
        }
        ComposerError::Transport(
            TransportError::BufferInFlight(_) | TransportError::FrameIdExhausted,
        ) => ErrorCode::NoResources,
        ComposerError::MissingClientTarget
        | ComposerError::DisplayOff
        | ComposerError::NonMonotonicPresentation { .. }
        | ComposerError::MissingRefreshRate
        | ComposerError::Transport(_) => ErrorCode::BadParameter,
    }
}

/// Re-export the format type used when configuring a session from DMA-BUF
/// feedback, keeping the Android adapter free from Cargo-only type discovery.
pub type AdvertisedFormat = FormatModifier;

#[cfg(test)]
mod tests {
    use droidloom_transport::Configure;

    use super::*;
    use crate::{PowerMode, TaskId};

    const FORMAT: FormatModifier = FormatModifier {
        fourcc: 875_713_112,
        modifier: 0,
    };

    fn session() -> Session {
        let mut session = Session::default();
        for (task, display, package) in [
            (TaskId(10), DisplayId(100), "org.example.mail"),
            (TaskId(11), DisplayId(101), "org.example.maps"),
        ] {
            session
                .create_task_window(TaskWindowSpec {
                    task,
                    display,
                    package: package.into(),
                })
                .unwrap();
            let composer = session
                .windows_mut()
                .composer_for_display_mut(display)
                .unwrap();
            composer.replace_feedback([FORMAT]);
            composer.configure(Configure {
                serial: 7,
                width: 800,
                height: 600,
                refresh_millihz: 60_000,
            });
            composer.acknowledge_configure(7).unwrap();
            composer.set_power_mode(display, PowerMode::On).unwrap();
        }
        session
    }

    fn empty_command(display: DisplayId) -> DisplayCommand {
        DisplayCommand {
            display,
            layers: Vec::new(),
            client_target: None,
            validate_display: false,
            accept_display_changes: false,
            present_display: false,
            present_or_validate_display: false,
        }
    }

    #[test]
    fn command_batches_keep_task_displays_independent() {
        let mut session = session();
        let mail = session.create_layer(DisplayId(100), 3).unwrap();
        let maps = session.create_layer(DisplayId(101), 3).unwrap();
        let mut mail_command = empty_command(DisplayId(100));
        mail_command.layers.push(LayerPatch {
            layer: mail,
            composition: Some(Composition::Device),
            z: Some(4),
            protected_content: None,
            has_sideband_stream: false,
        });
        mail_command.validate_display = true;
        let mut maps_command = empty_command(DisplayId(101));
        maps_command.layers.push(LayerPatch {
            layer: maps,
            composition: Some(Composition::Cursor),
            z: Some(9),
            protected_content: None,
            has_sideband_stream: false,
        });
        maps_command.validate_display = true;

        let results = session.execute_commands(vec![mail_command, maps_command]);
        assert_eq!(results.len(), 2);
        assert!(matches!(
            &results[0],
            CommandResult::ChangedCompositionTypes {
                display: DisplayId(100),
                layers,
            } if layers[0].layer == mail && layers[0].composition == Composition::Client
        ));
        assert!(matches!(
            &results[1],
            CommandResult::ChangedCompositionTypes {
                display: DisplayId(101),
                layers,
            } if layers[0].layer == maps && layers[0].composition == Composition::Client
        ));
    }

    #[test]
    fn direct_surfaceflinger_present_needs_no_composer_target() {
        let mut session = session();
        session.create_layer(DisplayId(100), 3).unwrap();
        let mut command = empty_command(DisplayId(100));
        command.validate_display = true;
        command.accept_display_changes = true;
        command.present_display = true;
        let results = session.execute_commands(vec![command]);
        let Some(CommandResult::PresentDirect { display }) = results
            .iter()
            .find(|result| matches!(result, CommandResult::PresentDirect { .. }))
        else {
            panic!("expected present result");
        };
        assert_eq!(*display, DisplayId(100));
        assert_eq!(
            session
                .windows()
                .composer_for_display(DisplayId(100))
                .unwrap()
                .in_flight_count(),
            0
        );
    }

    #[test]
    fn bad_display_and_unsupported_sideband_are_indexed_errors() {
        let mut session = session();
        let layer = session.create_layer(DisplayId(100), 3).unwrap();
        let unknown = empty_command(DisplayId(999));
        let mut sideband = empty_command(DisplayId(100));
        sideband.layers.push(LayerPatch {
            layer,
            composition: Some(Composition::Sideband),
            z: None,
            protected_content: None,
            has_sideband_stream: true,
        });
        assert_eq!(
            session.execute_commands(vec![unknown, sideband]),
            [
                CommandResult::Error {
                    command_index: 0,
                    code: ErrorCode::BadDisplay,
                },
                CommandResult::Error {
                    command_index: 1,
                    code: ErrorCode::Unsupported,
                }
            ]
        );
    }
}
