//! Protocol-independent state machine for Droidloom DMA-BUF presentation.
//!
//! The Android Composer and the native transport probe use this state to
//! enforce configure acknowledgement, DMA-BUF feedback, explicit acquire
//! fences, and release-before-reuse. It contains no renderer or wire code.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Opaque buffer identity retained across `SurfaceFlinger` presents.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BufferId(pub u64);

/// Opaque frame identity used to match release fences.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FrameId(pub u64);

/// One DRM format/modifier pair advertised by Denial DMA-BUF feedback.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatModifier {
    /// DRM `FourCC` code.
    pub fourcc: u32,
    /// Explicit DRM modifier. `DRM_FORMAT_MOD_INVALID` is not accepted.
    pub modifier: u64,
}

/// Immutable metadata for a DMA-BUF allocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BufferMetadata {
    /// Stable allocation identity.
    pub id: BufferId,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// DRM format and explicit modifier.
    pub format: FormatModifier,
    /// Ordered plane descriptions. File descriptors are owned by the wire adapter.
    pub planes: Vec<PlaneMetadata>,
}

/// One plane in a multi-planar DMA-BUF allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaneMetadata {
    /// Zero-based DMA-BUF plane index.
    pub index: u32,
    /// Byte offset into this plane's DMA-BUF.
    pub offset: u32,
    /// Bytes between adjacent rows.
    pub stride: u32,
}

impl BufferMetadata {
    /// Validate dimensions, plane indexing, and bounds used by the v1 path.
    ///
    /// # Errors
    ///
    /// Rejects malformed or unbounded metadata before any descriptor transfer.
    pub fn validate(&self) -> Result<(), TransportError> {
        if self.width == 0 || self.height == 0 {
            return Err(TransportError::InvalidBuffer("dimensions must be non-zero"));
        }
        if self.width > 16_384 || self.height > 16_384 {
            return Err(TransportError::InvalidBuffer(
                "dimensions exceed the v1 16384-pixel bound",
            ));
        }
        if self.planes.is_empty() || self.planes.len() > 4 {
            return Err(TransportError::InvalidBuffer(
                "DMA-BUFs require one through four planes",
            ));
        }
        for (expected, plane) in self.planes.iter().enumerate() {
            if usize::try_from(plane.index).ok() != Some(expected) {
                return Err(TransportError::InvalidBuffer(
                    "plane indices must be dense and start at zero",
                ));
            }
            if plane.stride == 0 {
                return Err(TransportError::InvalidBuffer(
                    "plane stride must be non-zero",
                ));
            }
        }
        Ok(())
    }
}

/// One compositor configure that must be acknowledged before presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configure {
    /// Denial configure serial.
    pub serial: u32,
    /// Required buffer width for the initial client-target path.
    pub width: u32,
    /// Required buffer height for the initial client-target path.
    pub height: u32,
    /// Output refresh vote in millihertz.
    pub refresh_millihz: u32,
}

/// Damage rectangle in buffer coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Damage {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// Frame accepted for submission to Denial.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedFrame {
    /// Unique frame identity.
    pub frame_id: FrameId,
    /// Buffer whose descriptor is submitted.
    pub buffer: BufferMetadata,
    /// Buffer-coordinate damage forwarded to Denial.
    pub damage: Vec<Damage>,
    /// True by construction: the wire adapter must send an acquire sync-file.
    pub requires_acquire_fence: bool,
}

/// Buffer that became reusable only after a Denial release fence arrived.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReleasedFrame {
    /// Retired frame.
    pub frame_id: FrameId,
    /// Buffer now eligible for another present.
    pub buffer_id: BufferId,
    /// True by construction: the caller received a release sync-file.
    pub has_release_fence: bool,
}

/// Invalid protocol transition or buffer metadata.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TransportError {
    /// A buffer was submitted before the latest configure was acknowledged.
    #[error("latest configure has not been acknowledged")]
    Unconfigured,
    /// The supplied serial is not the current pending configure.
    #[error("configure serial {0} is unknown or stale")]
    InvalidSerial(u32),
    /// The buffer dimensions do not match the acknowledged configure.
    #[error(
        "buffer size {actual_width}x{actual_height} does not match {expected_width}x{expected_height}"
    )]
    SizeMismatch {
        /// Required width.
        expected_width: u32,
        /// Required height.
        expected_height: u32,
        /// Submitted width.
        actual_width: u32,
        /// Submitted height.
        actual_height: u32,
    },
    /// Denial feedback did not advertise the submitted combination.
    #[error("format {fourcc:#010x} modifier {modifier:#018x} is absent from DMA-BUF feedback")]
    UnsupportedFormat {
        /// DRM `FourCC`.
        fourcc: u32,
        /// DRM modifier.
        modifier: u64,
    },
    /// The allocation is still sampled, scanned out, or awaiting release.
    #[error("buffer {0:?} is still in flight")]
    BufferInFlight(BufferId),
    /// A release event did not name an in-flight frame.
    #[error("frame {0:?} is not in flight")]
    UnknownFrame(FrameId),
    /// The wire adapter omitted an explicit synchronization file.
    #[error("an explicit sync-file fence is required")]
    MissingFence,
    /// Metadata could not describe a safe bounded DMA-BUF import.
    #[error("invalid DMA-BUF: {0}")]
    InvalidBuffer(&'static str),
    /// Damage exceeded the submitted buffer.
    #[error("damage rectangle exceeds buffer bounds")]
    InvalidDamage,
    /// Internal frame ID space was exhausted.
    #[error("frame identifier space exhausted")]
    FrameIdExhausted,
}

/// One task/client-target presentation stream.
#[derive(Debug, Default)]
pub struct PresentationState {
    feedback: BTreeSet<FormatModifier>,
    latest_configure: Option<Configure>,
    acknowledged_serial: Option<u32>,
    in_flight: BTreeMap<FrameId, BufferId>,
    busy_buffers: BTreeSet<BufferId>,
    next_frame_id: u64,
}

impl PresentationState {
    /// Return the latest host configure, including size and refresh policy.
    pub fn latest_configure(&self) -> Option<Configure> {
        self.latest_configure
    }

    /// Return the buffer owned by an in-flight frame.
    pub fn in_flight_buffer(&self, frame: FrameId) -> Option<BufferId> {
        self.in_flight.get(&frame).copied()
    }

    /// Replace the authoritative Denial format/modifier feedback atomically.
    pub fn replace_feedback(&mut self, formats: impl IntoIterator<Item = FormatModifier>) {
        self.feedback = formats.into_iter().collect();
    }

    /// Record a new configure. A new serial invalidates an earlier acknowledgement.
    pub fn configure(&mut self, configure: Configure) {
        if self.latest_configure.map(|old| old.serial) != Some(configure.serial) {
            self.acknowledged_serial = None;
        }
        self.latest_configure = Some(configure);
    }

    /// Acknowledge exactly the latest configure.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidSerial`] for stale or fabricated serials.
    pub fn acknowledge(&mut self, serial: u32) -> Result<(), TransportError> {
        if self.latest_configure.map(|configure| configure.serial) != Some(serial) {
            return Err(TransportError::InvalidSerial(serial));
        }
        self.acknowledged_serial = Some(serial);
        Ok(())
    }

    /// Validate and reserve a buffer until an explicit release fence arrives.
    ///
    /// `has_acquire_fence` describes descriptor ownership in the wire adapter;
    /// the descriptor itself intentionally never enters this pure state machine.
    ///
    /// # Errors
    ///
    /// Rejects unconfigured, unsupported, unfenced, malformed, or reused buffers.
    pub fn submit(
        &mut self,
        buffer: BufferMetadata,
        damage: Vec<Damage>,
        has_acquire_fence: bool,
    ) -> Result<AcceptedFrame, TransportError> {
        let configure = self.latest_configure.ok_or(TransportError::Unconfigured)?;
        if self.acknowledged_serial != Some(configure.serial) {
            return Err(TransportError::Unconfigured);
        }
        buffer.validate()?;
        if (buffer.width, buffer.height) != (configure.width, configure.height) {
            return Err(TransportError::SizeMismatch {
                expected_width: configure.width,
                expected_height: configure.height,
                actual_width: buffer.width,
                actual_height: buffer.height,
            });
        }
        if !self.feedback.contains(&buffer.format) {
            return Err(TransportError::UnsupportedFormat {
                fourcc: buffer.format.fourcc,
                modifier: buffer.format.modifier,
            });
        }
        if !has_acquire_fence {
            return Err(TransportError::MissingFence);
        }
        if self.busy_buffers.contains(&buffer.id) {
            return Err(TransportError::BufferInFlight(buffer.id));
        }
        if damage.iter().any(|rect| !damage_fits(*rect, &buffer)) {
            return Err(TransportError::InvalidDamage);
        }

        let next_frame_id = self
            .next_frame_id
            .checked_add(1)
            .ok_or(TransportError::FrameIdExhausted)?;
        self.next_frame_id = next_frame_id;
        let frame_id = FrameId(next_frame_id);
        self.busy_buffers.insert(buffer.id);
        self.in_flight.insert(frame_id, buffer.id);
        Ok(AcceptedFrame {
            frame_id,
            buffer,
            damage,
            requires_acquire_fence: true,
        })
    }

    /// Retire a frame after receiving its explicit release sync-file.
    ///
    /// # Errors
    ///
    /// Rejects missing release fences and duplicate/unknown release events.
    pub fn release(
        &mut self,
        frame_id: FrameId,
        has_release_fence: bool,
    ) -> Result<ReleasedFrame, TransportError> {
        if !has_release_fence {
            return Err(TransportError::MissingFence);
        }
        let buffer_id = self
            .in_flight
            .remove(&frame_id)
            .ok_or(TransportError::UnknownFrame(frame_id))?;
        let removed = self.busy_buffers.remove(&buffer_id);
        debug_assert!(removed, "in-flight frame must have a busy buffer");
        Ok(ReleasedFrame {
            frame_id,
            buffer_id,
            has_release_fence: true,
        })
    }

    /// Roll back a frame which the platform adapter failed to submit to Denial.
    ///
    /// This is valid only before the `Present` record has been accepted by the
    /// host endpoint. Once Denial may sample the buffer, only [`Self::release`]
    /// with explicit-fence completion can make it reusable.
    ///
    /// # Errors
    ///
    /// Rejects an unknown or already retired frame.
    pub fn cancel_unsubmitted(&mut self, frame_id: FrameId) -> Result<BufferId, TransportError> {
        let buffer_id = self
            .in_flight
            .remove(&frame_id)
            .ok_or(TransportError::UnknownFrame(frame_id))?;
        let removed = self.busy_buffers.remove(&buffer_id);
        debug_assert!(removed, "in-flight frame must have a busy buffer");
        Ok(buffer_id)
    }

    /// Number of buffers that Denial still owns.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }
}

fn damage_fits(damage: Damage, buffer: &BufferMetadata) -> bool {
    damage.width > 0
        && damage.height > 0
        && damage
            .x
            .checked_add(damage.width)
            .is_some_and(|right| right <= buffer.width)
        && damage
            .y
            .checked_add(damage.height)
            .is_some_and(|bottom| bottom <= buffer.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    const XR24: u32 = u32::from_le_bytes(*b"XR24");
    const MODIFIER: u64 = 0x0100_0000_0000_0002;

    fn buffer(id: u64) -> BufferMetadata {
        BufferMetadata {
            id: BufferId(id),
            width: 1080,
            height: 2400,
            format: FormatModifier {
                fourcc: XR24,
                modifier: MODIFIER,
            },
            planes: vec![PlaneMetadata {
                index: 0,
                offset: 0,
                stride: 4_352,
            }],
        }
    }

    fn configured() -> PresentationState {
        let mut state = PresentationState::default();
        state.replace_feedback([buffer(0).format]);
        state.configure(Configure {
            serial: 7,
            width: 1080,
            height: 2400,
            refresh_millihz: 120_000,
        });
        state.acknowledge(7).unwrap();
        state
    }

    fn full_damage() -> Vec<Damage> {
        vec![Damage {
            x: 0,
            y: 0,
            width: 1080,
            height: 2400,
        }]
    }

    #[test]
    fn requires_latest_configure_acknowledgement() {
        let mut state = configured();
        state.configure(Configure {
            serial: 8,
            width: 1080,
            height: 2400,
            refresh_millihz: 120_000,
        });
        assert_eq!(
            state.submit(buffer(1), full_damage(), true),
            Err(TransportError::Unconfigured)
        );
        assert_eq!(state.acknowledge(7), Err(TransportError::InvalidSerial(7)));
    }

    #[test]
    fn latest_configure_is_queryable_without_acknowledging_it() {
        let mut state = PresentationState::default();
        assert_eq!(state.latest_configure(), None);
        let configure = Configure {
            serial: 9,
            width: 1440,
            height: 900,
            refresh_millihz: 90_000,
        };
        state.configure(configure);
        assert_eq!(state.latest_configure(), Some(configure));
    }

    #[test]
    fn rejects_implicit_sync_and_unadvertised_modifiers() {
        let mut state = configured();
        assert_eq!(
            state.submit(buffer(1), full_damage(), false),
            Err(TransportError::MissingFence)
        );
        let mut unsupported = buffer(2);
        unsupported.format.modifier = 0;
        assert!(matches!(
            state.submit(unsupported, full_damage(), true),
            Err(TransportError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn buffer_cannot_be_reused_before_release_fence() {
        let mut state = configured();
        let first = state.submit(buffer(11), full_damage(), true).unwrap();
        assert_eq!(state.in_flight_count(), 1);
        assert_eq!(
            state.submit(buffer(11), full_damage(), true),
            Err(TransportError::BufferInFlight(BufferId(11)))
        );
        assert_eq!(
            state.release(first.frame_id, false),
            Err(TransportError::MissingFence)
        );
        state.release(first.frame_id, true).unwrap();
        state.submit(buffer(11), full_damage(), true).unwrap();
    }

    #[test]
    fn unsubmitted_frame_can_be_rolled_back_without_a_fence() {
        let mut state = configured();
        let frame = state.submit(buffer(12), full_damage(), true).unwrap();
        assert_eq!(
            state.cancel_unsubmitted(frame.frame_id).unwrap(),
            BufferId(12)
        );
        assert_eq!(state.in_flight_count(), 0);
        state.submit(buffer(12), full_damage(), true).unwrap();
    }

    #[test]
    fn validates_plane_and_damage_bounds_before_reservation() {
        let mut state = configured();
        let mut malformed = buffer(1);
        malformed.planes[0].index = 1;
        assert!(matches!(
            state.submit(malformed, full_damage(), true),
            Err(TransportError::InvalidBuffer(_))
        ));
        assert_eq!(state.in_flight_count(), 0);

        let invalid_damage = vec![Damage {
            x: 1_000,
            y: 0,
            width: 100,
            height: 1,
        }];
        assert_eq!(
            state.submit(buffer(1), invalid_damage, true),
            Err(TransportError::InvalidDamage)
        );
    }
}
