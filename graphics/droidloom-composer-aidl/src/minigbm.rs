//! Safe minigbm native-handle and buffer-slot adapter for Composer V5.
//!
//! The adapter owns duplicated plane descriptors and acquire fences until a
//! presentation sink accepts them. It never fabricates a handle or fence.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;

use android_hardware_common::aidl::android::hardware::common::NativeHandle::NativeHandle;
use android_hardware_graphics_common::aidl::android::hardware::graphics::common::{
    BlendMode::BlendMode as AidlBlendMode, Transform::Transform as AidlTransform,
};
use android_hardware_graphics_composer3::aidl::android::hardware::graphics::composer3::{
    Buffer::Buffer as AidlBuffer, Composition::Composition as AidlComposition,
    LayerCommand::LayerCommand as AidlLayerCommand,
    LayerLifecycleBatchCommandType::LayerLifecycleBatchCommandType,
};
use binder::ParcelFileDescriptor;
use droidloom_composer::{
    hwc3::{ErrorCode, MAX_BUFFER_SLOTS},
    ClientTarget, DisplayId, LayerId,
};
use droidloom_minigbm::{parse_minigbm_handle, MinigbmMetadata};
use droidloom_transport::{AcceptedFrame, BufferMetadata, Damage};

use crate::{NativeBufferAdapter, PresentationFailure};

const COMPOSITION_CLIENT: i32 = 0;
const COMPOSITION_DEVICE: i32 = 1;
const COMPOSITION_CURSOR: i32 = 2;
const COMPOSITION_SOLID_COLOR: i32 = 3;

/// One validated minigbm allocation with owned duplicates of its image-plane
/// descriptors. The optional reserved-region descriptor is deliberately not
/// retained because native presentation consumes only image planes.
pub struct ImportedMinigbmBuffer {
    /// Exact parsed metadata from the pinned minigbm handle ABI.
    pub metadata: MinigbmMetadata,
    /// Image-plane descriptors in dense DMA-BUF plane order.
    pub plane_fds: Vec<ParcelFileDescriptor>,
}

impl ImportedMinigbmBuffer {
    fn try_clone(&self) -> Result<Self, ErrorCode> {
        let plane_fds = self
            .plane_fds
            .iter()
            .map(|fd| fd.try_clone().map_err(|_| ErrorCode::NoResources))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            metadata: self.metadata.clone(),
            plane_fds,
        })
    }
}

/// One Denial-owned target imported from the private host protocol.
pub struct ImportedRenderTarget {
    /// Immutable DMA-BUF allocation metadata.
    pub metadata: BufferMetadata,
    /// Denial configure serial whose size this target satisfies.
    pub configure_serial: u32,
    /// Owned duplicates of all dense image-plane descriptors.
    pub plane_fds: Vec<OwnedFd>,
}

/// Integer display-frame rectangle in task-target coordinates.
#[derive(Clone, Copy, Debug)]
pub struct LayerRect {
    /// Left edge.
    pub left: i32,
    /// Top edge.
    pub top: i32,
    /// Right edge.
    pub right: i32,
    /// Bottom edge.
    pub bottom: i32,
}

/// Floating source crop in source-buffer coordinates.
#[derive(Clone, Copy, Debug)]
pub struct LayerCrop {
    /// Left edge.
    pub left: f32,
    /// Top edge.
    pub top: f32,
    /// Right edge.
    pub right: f32,
    /// Bottom edge.
    pub bottom: f32,
}

/// One complete layer snapshot consumed by the Droidloom compositor.
pub struct PreparedLayer {
    /// SurfaceFlinger layer identity.
    pub layer: LayerId,
    /// Buffer and plane descriptors for DEVICE layers.
    pub buffer: Option<ImportedMinigbmBuffer>,
    /// Latest acquire fence, consumed by this composition.
    pub acquire_fence: Option<ParcelFileDescriptor>,
    /// Display destination.
    pub display_frame: LayerRect,
    /// Buffer source crop.
    pub source_crop: LayerCrop,
    /// Android transform bit field.
    pub transform: i32,
    /// Android blend-mode value.
    pub blend_mode: i32,
    /// Whole-layer alpha.
    pub plane_alpha: f32,
    /// Solid RGBA color when the layer has no buffer.
    pub solid_color: Option<[f32; 4]>,
    /// Layer stacking order.
    pub z: i32,
}

/// Native presentation boundary implemented by the Denial protocol adapter.
pub trait PresentationSink {
    /// Reserve one free Denial-owned target for a task display.
    fn acquire_target(&mut self, display: DisplayId) -> Result<ImportedRenderTarget, ErrorCode>;

    /// Return a target reservation that did not reach presentation.
    fn cancel_target(&mut self, display: DisplayId, target: ImportedRenderTarget);

    /// Take the SurfaceFlinger completion fence for a direct-rendered frame.
    fn take_direct_present_fence(
        &mut self,
        display: DisplayId,
    ) -> Result<ParcelFileDescriptor, ErrorCode>;

    /// Compose layers into the target, submit it to Denial, and return the
    /// real present fence for SurfaceFlinger.
    fn submit(
        &mut self,
        display: DisplayId,
        frame: AcceptedFrame,
        target: ImportedRenderTarget,
        layers: Vec<PreparedLayer>,
    ) -> Result<ParcelFileDescriptor, PresentationFailure>;
}

/// Bounded per-client native-buffer adapter for the pinned minigbm allocator.
pub struct MinigbmBufferAdapter<S> {
    sink: S,
    client_target_slot_counts: BTreeMap<DisplayId, u32>,
    layers: BTreeMap<(DisplayId, LayerId), LayerRenderState>,
    pending_targets: BTreeMap<DisplayId, PendingTarget>,
}

impl<S> MinigbmBufferAdapter<S> {
    /// Create an empty Binder-client adapter around a native presentation sink.
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            client_target_slot_counts: BTreeMap::new(),
            layers: BTreeMap::new(),
            pending_targets: BTreeMap::new(),
        }
    }

    /// Borrow the presentation sink for lifecycle and release-event plumbing.
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    /// Consume the adapter and return its presentation sink.
    pub fn into_sink(self) -> S {
        self.sink
    }
}

impl<S> NativeBufferAdapter for MinigbmBufferAdapter<S>
where
    S: PresentationSink,
{
    fn create_layer_slots(
        &mut self,
        display: DisplayId,
        layer: LayerId,
        count: u32,
    ) -> Result<(), ErrorCode> {
        let key = (display, layer);
        if self.layers.contains_key(&key) {
            return Err(ErrorCode::BadLayer);
        }
        self.layers.insert(key, LayerRenderState::new(count)?);
        Ok(())
    }

    fn destroy_layer_slots(&mut self, display: DisplayId, layer: LayerId) {
        self.layers.remove(&(display, layer));
    }

    fn set_client_target_slot_count(
        &mut self,
        display: DisplayId,
        count: u32,
    ) -> Result<(), ErrorCode> {
        if self.pending_targets.contains_key(&display) {
            return Err(ErrorCode::BadParameter);
        }
        if count == 0 || count > MAX_BUFFER_SLOTS {
            return Err(ErrorCode::BadParameter);
        }
        self.client_target_slot_counts.insert(display, count);
        Ok(())
    }

    fn prepare_render_target(&mut self, display: DisplayId) -> Result<ClientTarget, ErrorCode> {
        if self.pending_targets.contains_key(&display) {
            return Err(ErrorCode::BadParameter);
        }
        let target = self.sink.acquire_target(display)?;
        let damage = vec![Damage {
            x: 0,
            y: 0,
            width: target.metadata.width,
            height: target.metadata.height,
        }];
        let core_target = ClientTarget {
            buffer: target.metadata.clone(),
            damage,
            has_acquire_fence: true,
        };
        self.pending_targets
            .insert(display, PendingTarget { target });
        Ok(core_target)
    }

    fn cancel_render_target(&mut self, display: DisplayId) {
        if let Some(pending) = self.pending_targets.remove(&display) {
            self.sink.cancel_target(display, pending.target);
        }
    }

    fn take_direct_present_fence(
        &mut self,
        display: DisplayId,
    ) -> Result<ParcelFileDescriptor, ErrorCode> {
        self.sink.take_direct_present_fence(display)
    }

    fn update_layer(
        &mut self,
        display: DisplayId,
        layer: LayerId,
        command: &AidlLayerCommand,
    ) -> Result<Option<bool>, ErrorCode> {
        if command.layerLifecycleBatchCommandType != LayerLifecycleBatchCommandType::MODIFY
            || command.newBufferSlotCount != 0
        {
            return Err(ErrorCode::Unsupported);
        }
        let state = self
            .layers
            .get_mut(&(display, layer))
            .ok_or(ErrorCode::BadLayer)?;
        if let Some(to_clear) = &command.bufferSlotsToClear {
            state.slots.clear(to_clear)?;
        }
        let protected = if let Some(buffer) = &command.buffer {
            let imported = state.slots.resolve(buffer)?;
            let protected = imported.metadata.is_protected();
            state.buffer = Some(imported);
            state.acquire_fence = buffer
                .fence
                .as_ref()
                .map(|fence| fence.try_clone().map_err(|_| ErrorCode::NoResources))
                .transpose()?;
            Some(protected)
        } else {
            None
        };
        state.apply(command)?;
        Ok(protected)
    }

    fn submit_composed_target(
        &mut self,
        display: DisplayId,
        frame: AcceptedFrame,
    ) -> Result<ParcelFileDescriptor, PresentationFailure> {
        let pending = self
            .pending_targets
            .remove(&display)
            .ok_or_else(|| PresentationFailure::unsubmitted(ErrorCode::BadParameter))?;
        if !frame.requires_acquire_fence || pending.target.metadata != frame.buffer {
            self.sink.cancel_target(display, pending.target);
            return Err(PresentationFailure::unsubmitted(ErrorCode::BadParameter));
        }
        let layers = match prepare_layers(display, &mut self.layers) {
            Ok(layers) => layers,
            Err(code) => {
                self.sink.cancel_target(display, pending.target);
                return Err(PresentationFailure::unsubmitted(code));
            }
        };
        self.sink.submit(display, frame, pending.target, layers)
    }
}

struct PendingTarget {
    target: ImportedRenderTarget,
}

struct LayerRenderState {
    slots: DescriptorSlots,
    buffer: Option<ImportedMinigbmBuffer>,
    acquire_fence: Option<ParcelFileDescriptor>,
    display_frame: Option<LayerRect>,
    source_crop: Option<LayerCrop>,
    transform: i32,
    blend_mode: i32,
    plane_alpha: f32,
    solid_color: Option<[f32; 4]>,
    z: i32,
    composition: i32,
}

impl LayerRenderState {
    fn new(count: u32) -> Result<Self, ErrorCode> {
        Ok(Self {
            slots: DescriptorSlots::new(count)?,
            buffer: None,
            acquire_fence: None,
            display_frame: None,
            source_crop: None,
            transform: 0,
            blend_mode: 1,
            plane_alpha: 1.0,
            solid_color: None,
            z: 0,
            composition: 0,
        })
    }

    fn apply(&mut self, command: &AidlLayerCommand) -> Result<(), ErrorCode> {
        if let Some(composition) = &command.composition {
            self.composition = composition_code(composition.composition)?;
            if self.composition == COMPOSITION_SOLID_COLOR {
                // A solid-color layer has no source buffer. Discard a crop
                // retained from an earlier composition type so it cannot be
                // interpreted as texture coordinates during presentation.
                self.source_crop = None;
            }
        }
        if let Some(frame) = &command.displayFrame {
            if frame.left < 0
                || frame.top < 0
                || frame.right <= frame.left
                || frame.bottom <= frame.top
            {
                return Err(ErrorCode::BadParameter);
            }
            self.display_frame = Some(LayerRect {
                left: frame.left,
                top: frame.top,
                right: frame.right,
                bottom: frame.bottom,
            });
        }
        if let Some(crop) = &command.sourceCrop {
            self.source_crop = normalize_source_crop(
                self.composition,
                [crop.left, crop.top, crop.right, crop.bottom],
            )?;
        }
        if let Some(transform) = &command.transform {
            self.transform = transform_code(transform.transform)?;
        }
        if let Some(blend) = &command.blendMode {
            self.blend_mode = blend_code(blend.blendMode)?;
        }
        if let Some(alpha) = &command.planeAlpha {
            if !alpha.alpha.is_finite() || !(0.0..=1.0).contains(&alpha.alpha) {
                return Err(ErrorCode::BadParameter);
            }
            self.plane_alpha = alpha.alpha;
        }
        if let Some(color) = &command.color {
            let components = [color.r, color.g, color.b, color.a];
            if components
                .iter()
                .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            {
                return Err(ErrorCode::BadParameter);
            }
            self.solid_color = Some(components);
        }
        if let Some(z) = &command.z {
            self.z = z.z;
        }
        Ok(())
    }
}

fn prepare_layers(
    display: DisplayId,
    layers: &mut BTreeMap<(DisplayId, LayerId), LayerRenderState>,
) -> Result<Vec<PreparedLayer>, ErrorCode> {
    let mut prepared = Vec::new();
    for ((owned_display, layer), state) in layers.iter_mut() {
        if *owned_display != display {
            continue;
        }
        let solid = state.composition == COMPOSITION_SOLID_COLOR;
        if !solid && state.buffer.is_none() {
            continue;
        }
        let display_frame = state.display_frame.ok_or(ErrorCode::BadParameter)?;
        let buffer = if solid {
            None
        } else {
            Some(
                state
                    .buffer
                    .as_ref()
                    .ok_or(ErrorCode::BadParameter)?
                    .try_clone()?,
            )
        };
        let source_crop = match (state.source_crop, buffer.as_ref()) {
            (Some(crop), _) => crop,
            (None, Some(buffer)) => LayerCrop {
                left: 0.0,
                top: 0.0,
                right: buffer.metadata.buffer.width as f32,
                bottom: buffer.metadata.buffer.height as f32,
            },
            (None, None) => LayerCrop {
                left: 0.0,
                top: 0.0,
                right: (display_frame.right - display_frame.left) as f32,
                bottom: (display_frame.bottom - display_frame.top) as f32,
            },
        };
        prepared.push(PreparedLayer {
            layer: *layer,
            buffer,
            acquire_fence: state.acquire_fence.take(),
            display_frame,
            source_crop,
            transform: state.transform,
            blend_mode: state.blend_mode,
            plane_alpha: state.plane_alpha,
            solid_color: if solid { state.solid_color } else { None },
            z: state.z,
        });
    }
    prepared.sort_by_key(|layer| (layer.z, layer.layer.0));
    Ok(prepared)
}

fn composition_code(composition: AidlComposition) -> Result<i32, ErrorCode> {
    if composition == AidlComposition::CLIENT {
        Ok(COMPOSITION_CLIENT)
    } else if composition == AidlComposition::DEVICE {
        Ok(COMPOSITION_DEVICE)
    } else if composition == AidlComposition::CURSOR {
        Ok(COMPOSITION_CURSOR)
    } else if composition == AidlComposition::SOLID_COLOR {
        Ok(COMPOSITION_SOLID_COLOR)
    } else {
        Err(ErrorCode::Unsupported)
    }
}

fn normalize_source_crop(composition: i32, crop: [f32; 4]) -> Result<Option<LayerCrop>, ErrorCode> {
    // Composer3 permits an empty source crop for SOLID_COLOR because that
    // composition type has no source buffer. SurfaceFlinger uses exactly this
    // representation for letterbox bars around fixed-orientation apps.
    if composition == COMPOSITION_SOLID_COLOR {
        return Ok(None);
    }
    let [left, top, right, bottom] = crop;
    if !left.is_finite()
        || !top.is_finite()
        || !right.is_finite()
        || !bottom.is_finite()
        || left < 0.0
        || top < 0.0
        || right <= left
        || bottom <= top
    {
        return Err(ErrorCode::BadParameter);
    }
    Ok(Some(LayerCrop {
        left,
        top,
        right,
        bottom,
    }))
}

fn transform_code(transform: AidlTransform) -> Result<i32, ErrorCode> {
    if transform == AidlTransform::NONE {
        Ok(0)
    } else if transform == AidlTransform::FLIP_H {
        Ok(1)
    } else if transform == AidlTransform::FLIP_V {
        Ok(2)
    } else if transform == AidlTransform::ROT_90 {
        Ok(4)
    } else if transform == AidlTransform::ROT_180 {
        Ok(3)
    } else if transform == AidlTransform::ROT_270 {
        Ok(7)
    } else {
        Err(ErrorCode::Unsupported)
    }
}

fn blend_code(blend: AidlBlendMode) -> Result<i32, ErrorCode> {
    if blend == AidlBlendMode::NONE {
        Ok(1)
    } else if blend == AidlBlendMode::PREMULTIPLIED {
        Ok(2)
    } else if blend == AidlBlendMode::COVERAGE {
        Ok(3)
    } else {
        Err(ErrorCode::Unsupported)
    }
}

struct DescriptorSlots {
    entries: Vec<Option<ImportedMinigbmBuffer>>,
}

impl DescriptorSlots {
    fn new(count: u32) -> Result<Self, ErrorCode> {
        Ok(Self {
            entries: empty_slots(count)?,
        })
    }

    fn resolve(&mut self, buffer: &AidlBuffer) -> Result<ImportedMinigbmBuffer, ErrorCode> {
        let slot = slot_index(buffer.slot, self.entries.len())?;
        if let Some(handle) = &buffer.handle {
            let imported = import_handle(handle)?;
            let pending = imported.try_clone()?;
            self.entries[slot] = Some(imported);
            Ok(pending)
        } else {
            self.entries[slot]
                .as_ref()
                .ok_or(ErrorCode::BadParameter)?
                .try_clone()
        }
    }

    fn clear(&mut self, slots: &[i32]) -> Result<(), ErrorCode> {
        if slots.len() > usize::try_from(MAX_BUFFER_SLOTS).unwrap_or(usize::MAX) {
            return Err(ErrorCode::BadParameter);
        }
        let indices = slots
            .iter()
            .map(|slot| slot_index(*slot, self.entries.len()))
            .collect::<Result<Vec<_>, _>>()?;
        for index in indices {
            self.entries[index] = None;
        }
        Ok(())
    }
}

fn empty_slots<T>(count: u32) -> Result<Vec<Option<T>>, ErrorCode> {
    if count == 0 || count > MAX_BUFFER_SLOTS {
        return Err(ErrorCode::BadParameter);
    }
    let count = usize::try_from(count).map_err(|_| ErrorCode::BadParameter)?;
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(count)
        .map_err(|_| ErrorCode::NoResources)?;
    slots.resize_with(count, || None);
    Ok(slots)
}

fn slot_index(slot: i32, count: usize) -> Result<usize, ErrorCode> {
    usize::try_from(slot)
        .ok()
        .filter(|slot| *slot < count)
        .ok_or(ErrorCode::BadParameter)
}

fn parse_handle(handle: &NativeHandle) -> Result<MinigbmMetadata, ErrorCode> {
    parse_minigbm_handle(handle.fds.len(), &handle.ints).map_err(|_| ErrorCode::BadParameter)
}

fn import_handle(handle: &NativeHandle) -> Result<ImportedMinigbmBuffer, ErrorCode> {
    let metadata = parse_handle(handle)?;
    let plane_fds = handle
        .fds
        .iter()
        .take(metadata.plane_fd_count)
        .map(|fd| fd.try_clone().map_err(|_| ErrorCode::NoResources))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ImportedMinigbmBuffer {
        metadata,
        plane_fds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_color_ignores_surface_flinger_empty_source_crop() {
        assert!(
            normalize_source_crop(COMPOSITION_SOLID_COLOR, [0.0, 0.0, 0.0, 0.0])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn buffered_layer_still_rejects_empty_source_crop() {
        assert_eq!(
            normalize_source_crop(COMPOSITION_DEVICE, [0.0, 0.0, 0.0, 0.0]).unwrap_err(),
            ErrorCode::BadParameter
        );
    }

    #[test]
    fn buffered_layer_keeps_valid_source_crop() {
        let crop = normalize_source_crop(COMPOSITION_DEVICE, [1.0, 2.0, 101.0, 202.0])
            .unwrap()
            .unwrap();
        assert_eq!((crop.left, crop.top), (1.0, 2.0));
        assert_eq!((crop.right, crop.bottom), (101.0, 202.0));
    }
}
