//! Frozen Composer V5 AIDL boundary for the Droidloom vendor HAL.
//!
//! This crate is intentionally built only by Android Soong. It joins the exact
//! Composer V5 Rust bindings to the platform-independent core, minigbm handle
//! ownership, the Denial protocol socket, and shared DRM-syncobj timelines
//! without making Binder a host Cargo dependency.

#![deny(unsafe_code)]

pub mod denial;
#[allow(unsafe_code)]
pub mod gles;
pub mod minigbm;
pub mod service;

use android_hardware_graphics_composer3::aidl::android::hardware::graphics::composer3::{
    ChangedCompositionLayer::ChangedCompositionLayer,
    ChangedCompositionTypes::ChangedCompositionTypes,
    CommandError::CommandError,
    CommandResultPayload::CommandResultPayload,
    Composition::Composition as AidlComposition,
    DisplayCommand::DisplayCommand as AidlDisplayCommand,
    IComposer,
    LayerCommand::LayerCommand as AidlLayerCommand,
    ParcelableComposition::ParcelableComposition,
    PresentFence::PresentFence,
    PresentOrValidate::{PresentOrValidate, Result::Result as AidlPresentOrValidateResult},
};
use droidloom_composer::{
    hwc3::{
        CommandResult, DisplayCommand, ErrorCode, LayerPatch, PresentOrValidateResult, Session,
        MAX_DISPLAY_COMMANDS,
    },
    ClientTarget, Composition, DisplayId, LayerId,
};

/// Composer AIDL version selected by the Droidloom product.
pub const COMPOSER_AIDL_VERSION: i32 = IComposer::VERSION;

/// Frozen interface hash selected by the Droidloom product.
pub const COMPOSER_AIDL_HASH: &str = IComposer::HASH;

/// Presentation failure annotated with whether Denial may already own the
/// submitted buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationFailure {
    /// Frozen Composer service-specific error returned to `SurfaceFlinger`.
    pub code: ErrorCode,
    /// True only after one complete `Present` record reached the host socket.
    pub accepted_by_denial: bool,
}

impl PresentationFailure {
    /// Failure before Denial could observe the frame; local ownership may be
    /// rolled back without a release fence.
    pub const fn unsubmitted(code: ErrorCode) -> Self {
        Self {
            code,
            accepted_by_denial: false,
        }
    }

    /// Failure after Denial accepted the frame; buffer ownership must remain
    /// in flight until teardown or an explicit release event.
    pub const fn accepted(code: ErrorCode) -> Self {
        Self {
            code,
            accepted_by_denial: true,
        }
    }
}

/// Platform-owned buffer and synchronization operations needed by the safe
/// command adapter.
///
/// The implementation must retain imported DMA-BUF descriptors and buffer-slot
/// state until Denial returns a release fence. The AIDL bridge never guesses
/// gralloc metadata or fabricates synchronization descriptors.
pub trait NativeBufferAdapter {
    /// Create the bounded layer-buffer slot table paired with a new layer.
    fn create_layer_slots(
        &mut self,
        display: DisplayId,
        layer: LayerId,
        count: u32,
    ) -> Result<(), ErrorCode>;

    /// Remove the layer-buffer slot table after the core destroys its layer.
    fn destroy_layer_slots(&mut self, display: DisplayId, layer: LayerId);

    /// Resize Surface Flinger's client-target slot table for one task display.
    fn set_client_target_slot_count(
        &mut self,
        display: DisplayId,
        count: u32,
    ) -> Result<(), ErrorCode>;

    /// Reserve one Denial-owned target for a possible present command.
    fn prepare_render_target(&mut self, display: DisplayId) -> Result<ClientTarget, ErrorCode>;

    /// Cancel a reservation when `presentOrValidate` validated instead.
    fn cancel_render_target(&mut self, display: DisplayId);

    /// Take the SurfaceFlinger render fence paired with a direct present.
    fn take_direct_present_fence(
        &mut self,
        display: DisplayId,
    ) -> Result<binder::ParcelFileDescriptor, ErrorCode>;

    /// Import and retain one layer command for Droidloom composition, returning
    /// whether its current buffer requests protected presentation.
    fn update_layer(
        &mut self,
        display: DisplayId,
        layer: LayerId,
        command: &AidlLayerCommand,
    ) -> Result<Option<bool>, ErrorCode>;

    /// Compose the cached Android layers into the accepted target and submit it.
    fn submit_composed_target(
        &mut self,
        display: DisplayId,
        frame: droidloom_transport::AcceptedFrame,
    ) -> Result<binder::ParcelFileDescriptor, PresentationFailure>;
}

/// Convert one core service error into its frozen AIDL result payload.
pub fn command_error(command_index: u32, code: ErrorCode) -> CommandResultPayload {
    CommandResultPayload::Error(CommandError {
        commandIndex: i32::try_from(command_index).unwrap_or(i32::MAX),
        errorCode: code as i32,
    })
}

/// Convert the composition subset understood by the first client-target path.
pub fn composition(value: Composition) -> ParcelableComposition {
    let composition = match value {
        Composition::Client => AidlComposition::CLIENT,
        Composition::Device => AidlComposition::DEVICE,
        Composition::SolidColor => AidlComposition::SOLID_COLOR,
        Composition::Cursor => AidlComposition::CURSOR,
        Composition::Sideband => AidlComposition::SIDEBAND,
    };
    ParcelableComposition { composition }
}

/// Convert the indexed error subset of a core command result.
///
/// Frame and fence-bearing results remain with the native Denial wire
/// adapter, which must transfer descriptor ownership rather than fabricate it.
pub fn error_result(result: &CommandResult) -> Option<CommandResultPayload> {
    match result {
        CommandResult::Error {
            command_index,
            code,
        } => Some(command_error(*command_index, *code)),
        _ => None,
    }
}

/// Execute one frozen Composer V5 command batch against the safe session.
///
/// Translation failures are reported at the original AIDL command index. A
/// bad task display cannot corrupt another task window later in the batch.
pub fn execute_commands(
    session: &mut Session,
    commands: &[AidlDisplayCommand],
    buffers: &mut impl NativeBufferAdapter,
) -> Vec<CommandResultPayload> {
    if commands.len() > MAX_DISPLAY_COMMANDS {
        return vec![command_error(
            u32::try_from(MAX_DISPLAY_COMMANDS).unwrap_or(u32::MAX),
            ErrorCode::BadParameter,
        )];
    }

    let mut payloads = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let command_index = u32::try_from(index).unwrap_or(u32::MAX);
        let display = match display_id(command.display) {
            Ok(display) => display,
            Err(code) => {
                payloads.push(command_error(command_index, code));
                continue;
            }
        };
        if session.windows().composer_for_display(display).is_err() {
            payloads.push(command_error(command_index, ErrorCode::BadDisplay));
            continue;
        }
        let normalized = match normalize_command(display, command, buffers) {
            Ok(command) => command,
            Err(code) => {
                payloads.push(command_error(command_index, code));
                continue;
            }
        };

        let results = session.execute_commands(vec![normalized]);
        for result in results {
            match result_payload(result, command_index, session, buffers) {
                Ok(payload) => payloads.push(payload),
                Err(code) => payloads.push(command_error(command_index, code)),
            }
        }
    }
    payloads
}

fn normalize_command(
    display: DisplayId,
    command: &AidlDisplayCommand,
    buffers: &mut impl NativeBufferAdapter,
) -> Result<DisplayCommand, ErrorCode> {
    if command
        .colorTransformMatrix
        .as_deref()
        .is_some_and(|matrix| !is_identity_color_transform(matrix))
        || command.brightness.is_some()
        || command.virtualDisplayOutputBuffer.is_some()
        || command.activeConfig.is_some()
        || command.pictureProfileId != 0
    {
        return Err(ErrorCode::Unsupported);
    }

    let layers = command
        .layers
        .iter()
        .map(|layer| normalize_layer(display, layer, buffers))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DisplayCommand {
        display,
        layers,
        client_target: None,
        validate_display: command.validateDisplay,
        accept_display_changes: command.acceptDisplayChanges,
        present_display: command.presentDisplay,
        present_or_validate_display: command.presentOrValidateDisplay,
    })
}

fn is_identity_color_transform(matrix: &[f32]) -> bool {
    const IDENTITY: [f32; 16] = [
        1.0, 0.0, 0.0, 0.0, //
        0.0, 1.0, 0.0, 0.0, //
        0.0, 0.0, 1.0, 0.0, //
        0.0, 0.0, 0.0, 1.0,
    ];
    matrix == IDENTITY
}

fn normalize_layer(
    display: DisplayId,
    command: &AidlLayerCommand,
    buffers: &mut impl NativeBufferAdapter,
) -> Result<LayerPatch, ErrorCode> {
    let layer = layer_id(command.layer)?;
    let composition = command
        .composition
        .as_ref()
        .map(|value| core_composition(value.composition))
        .transpose()?;
    let protected_content = buffers.update_layer(display, layer, command)?;

    Ok(LayerPatch {
        layer,
        composition,
        z: command.z.as_ref().map(|z| z.z),
        protected_content,
        has_sideband_stream: command.sidebandStream.is_some(),
    })
}

fn result_payload(
    result: CommandResult,
    command_index: u32,
    session: &mut Session,
    buffers: &mut impl NativeBufferAdapter,
) -> Result<CommandResultPayload, ErrorCode> {
    match result {
        CommandResult::Error { code, .. } => Ok(command_error(command_index, code)),
        CommandResult::ChangedCompositionTypes { display, layers } => {
            let layers = layers
                .into_iter()
                .map(|change| {
                    Ok(ChangedCompositionLayer {
                        layer: aidl_id(change.layer.0)?,
                        composition: composition(change.composition).composition,
                    })
                })
                .collect::<Result<Vec<_>, ErrorCode>>()?;
            Ok(CommandResultPayload::ChangedCompositionTypes(
                ChangedCompositionTypes {
                    display: aidl_id(display.0)?,
                    layers,
                },
            ))
        }
        CommandResult::Present {
            display,
            frame,
            requires_present_fence,
        } => {
            if !requires_present_fence {
                return Err(ErrorCode::BadParameter);
            }
            let frame_id = frame.frame_id;
            let fence = match buffers.submit_composed_target(display, frame) {
                Ok(fence) => fence,
                Err(failure) => {
                    if !failure.accepted_by_denial {
                        session
                            .windows_mut()
                            .composer_for_display_mut(display)
                            .map_err(|_| ErrorCode::BadDisplay)?
                            .cancel_unsubmitted_target(frame_id)
                            .map_err(|_| ErrorCode::BadParameter)?;
                    }
                    return Err(failure.code);
                }
            };
            Ok(CommandResultPayload::PresentFence(PresentFence {
                display: aidl_id(display.0)?,
                fence: Some(fence),
                layerPresentFences: None,
            }))
        }
        CommandResult::PresentDirect { display } => {
            let fence = buffers.take_direct_present_fence(display)?;
            Ok(CommandResultPayload::PresentFence(PresentFence {
                display: aidl_id(display.0)?,
                fence: Some(fence),
                layerPresentFences: None,
            }))
        }
        CommandResult::PresentOrValidate { display, result } => {
            let result = match result {
                PresentOrValidateResult::Presented => AidlPresentOrValidateResult::Presented,
                PresentOrValidateResult::Validated => AidlPresentOrValidateResult::Validated,
            };
            Ok(CommandResultPayload::PresentOrValidateResult(
                PresentOrValidate {
                    display: aidl_id(display.0)?,
                    result,
                },
            ))
        }
    }
}

fn display_id(value: i64) -> Result<DisplayId, ErrorCode> {
    u64::try_from(value)
        .map(DisplayId)
        .map_err(|_| ErrorCode::BadDisplay)
}

fn layer_id(value: i64) -> Result<LayerId, ErrorCode> {
    u64::try_from(value)
        .map(LayerId)
        .map_err(|_| ErrorCode::BadLayer)
}

fn aidl_id(value: u64) -> Result<i64, ErrorCode> {
    i64::try_from(value).map_err(|_| ErrorCode::NoResources)
}

fn core_composition(value: AidlComposition) -> Result<Composition, ErrorCode> {
    if value == AidlComposition::CLIENT {
        Ok(Composition::Client)
    } else if value == AidlComposition::DEVICE {
        Ok(Composition::Device)
    } else if value == AidlComposition::SOLID_COLOR {
        Ok(Composition::SolidColor)
    } else if value == AidlComposition::CURSOR {
        Ok(Composition::Cursor)
    } else if value == AidlComposition::SIDEBAND {
        Ok(Composition::Sideband)
    } else {
        Err(ErrorCode::Unsupported)
    }
}
