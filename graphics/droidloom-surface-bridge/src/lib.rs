//! Private `SurfaceFlinger`-to-Composer transport for direct host targets.
//!
//! This protocol exists only inside one Android cell. Composer remains the
//! owner of the authenticated host connection and display bootstrap, while
//! `SurfaceFlinger` acquires a host-owned DMA-BUF and renders the complete
//! built-in display directly into it.

#![forbid(unsafe_code)]

use thiserror::Error;

/// Four-byte little-endian record marker.
pub const MAGIC: [u8; 4] = *b"DLSF";
/// Exact protocol version.
pub const VERSION: u16 = 3;
/// Fixed record header length.
pub const HEADER_BYTES: usize = 16;
/// Maximum image planes supported by both the host and Android paths.
pub const MAX_PLANES: usize = 4;
/// Largest encoded record in the private protocol.
pub const MAX_RECORD_BYTES: usize = 128;

mod opcode {
    pub const CLIENT_HELLO: u16 = 1;
    pub const SERVER_HELLO: u16 = 2;
    pub const ACQUIRE: u16 = 3;
    pub const TARGET: u16 = 4;
    pub const PRESENT: u16 = 5;
    pub const CANCEL: u16 = 6;
    pub const ACK: u16 = 7;
    pub const ERROR: u16 = 8;
    pub const RETIRE: u16 = 9;
    pub const PRESENT_WITH_CONTENT: u16 = 10;
    pub const PRESENT_WITH_DAMAGE: u16 = 11;
}

/// One DMA-BUF plane layout. Plane descriptors are attached in dense order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Plane {
    /// Byte offset in the allocation.
    pub offset: u32,
    /// Bytes between adjacent rows.
    pub stride: u32,
}

/// Bounding damage in target-buffer coordinates, relative to the preceding present.
/// Both dimensions zero represent an unchanged frame; absence means full damage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DamageRect {
    /// Left edge in pixels.
    pub x: u32,
    /// Top edge in pixels.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// `SurfaceFlinger` request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Negotiate the exact private protocol and select one presentation
    /// channel. Task zero selects the temporary whole-display bootstrap;
    /// positive values select a real Android task.
    Hello {
        /// Android `WindowManager` task identity, or zero for bootstrap.
        task: u64,
    },
    /// Reserve one idle host-owned target for a display.
    Acquire {
        /// Composer display identity.
        display: u64,
    },
    /// Submit a rendered target with zero (already complete) or one attached
    /// ready sync-file.
    Present {
        /// Composer display identity.
        display: u64,
        /// Reserved target identity.
        buffer: u64,
        /// True only when SurfaceFlinger proves full output coverage by opaque layers.
        opaque: bool,
        /// None means unknown/full damage, including legacy senders.
        damage: Option<DamageRect>,
    },
    /// Return an unsubmitted reservation.
    Cancel {
        /// Composer display identity.
        display: u64,
        /// Reserved target identity.
        buffer: u64,
    },
    /// Retire a task presentation channel after `WindowManager` destroyed it.
    Retire {
        /// Composer display identity selected by the hello transaction.
        display: u64,
    },
}

/// Composer response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Response {
    /// Publish the bootstrapped physical display geometry.
    Hello {
        /// Composer display identity.
        display: u64,
        /// Android display width in pixels.
        width: u32,
        /// Android display height in pixels.
        height: u32,
    },
    /// A reserved host-owned render target.
    Target {
        /// Composer display identity.
        display: u64,
        /// Stable allocation identity.
        buffer: u64,
        /// Host configure serial governing the allocation.
        configure_serial: u32,
        /// Allocation width.
        width: u32,
        /// Allocation height.
        height: u32,
        /// DRM fourcc.
        fourcc: u32,
        /// DRM format modifier.
        modifier: u64,
        /// Plane layouts matching the attached DMA-BUF descriptors.
        planes: Vec<Plane>,
    },
    /// The preceding mutation completed.
    Ack,
    /// Bounded failure code. Text is intentionally kept in Android logs.
    Error {
        /// Stable negative errno-like status.
        code: i32,
    },
}

/// Invalid record shape or value.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WireError {
    /// Header marker or protocol version did not match.
    #[error("invalid SurfaceFlinger bridge header")]
    Header,
    /// Record length did not match its header or opcode.
    #[error("invalid SurfaceFlinger bridge record length")]
    Length,
    /// Opcode is not defined by protocol version 1.
    #[error("unknown SurfaceFlinger bridge opcode")]
    Opcode,
    /// Descriptor count does not match the message contract.
    #[error("invalid SurfaceFlinger bridge descriptor count")]
    Descriptors,
    /// A field is outside the protocol contract.
    #[error("invalid SurfaceFlinger bridge field")]
    Field,
}

/// Decode and validate one client request plus its ancillary descriptor count.
///
/// # Errors
///
/// Returns [`WireError`] when the header, opcode, length, descriptor count, or
/// any request field violates the protocol contract.
pub fn decode_request(bytes: &[u8], descriptor_count: usize) -> Result<Request, WireError> {
    let (opcode, payload, advertised_descriptors) = decode_header(bytes)?;
    if advertised_descriptors != descriptor_count {
        return Err(WireError::Descriptors);
    }
    match opcode {
        opcode::CLIENT_HELLO if payload.len() == 8 && descriptor_count == 0 => Ok(Request::Hello {
            task: read_u64(payload, 0)?,
        }),
        opcode::ACQUIRE if payload.len() == 8 && descriptor_count == 0 => Ok(Request::Acquire {
            display: read_u64(payload, 0)?,
        }),
        opcode::PRESENT if payload.len() == 16 && descriptor_count <= 1 => Ok(Request::Present {
            display: read_u64(payload, 0)?,
            buffer: nonzero(read_u64(payload, 8)?)?,
            opaque: false,
            damage: None,
        }),
        opcode::PRESENT_WITH_CONTENT if payload.len() == 24 && descriptor_count <= 1 => {
            let flags = read_u32(payload, 16)?;
            if flags & !1 != 0 || read_u32(payload, 20)? != 0 { return Err(WireError::Field); }
            Ok(Request::Present {
                display: read_u64(payload, 0)?,
                buffer: nonzero(read_u64(payload, 8)?)?,
                opaque: flags & 1 != 0,
                damage: None,
            })
        }
        opcode::PRESENT_WITH_DAMAGE if payload.len() == 40 && descriptor_count <= 1 => {
            let flags = read_u32(payload, 16)?;
            let damage = DamageRect {
                x: read_u32(payload, 24)?,
                y: read_u32(payload, 28)?,
                width: read_u32(payload, 32)?,
                height: read_u32(payload, 36)?,
            };
            if flags & !1 != 0 || read_u32(payload, 20)? != 0
                || (damage.width == 0) != (damage.height == 0)
                || damage.x.checked_add(damage.width).is_none()
                || damage.y.checked_add(damage.height).is_none()
            {
                return Err(WireError::Field);
            }
            Ok(Request::Present {
                display: read_u64(payload, 0)?,
                buffer: nonzero(read_u64(payload, 8)?)?,
                opaque: flags & 1 != 0,
                damage: Some(damage),
            })
        }
        opcode::CANCEL if payload.len() == 16 && descriptor_count == 0 => Ok(Request::Cancel {
            display: read_u64(payload, 0)?,
            buffer: nonzero(read_u64(payload, 8)?)?,
        }),
        opcode::RETIRE if payload.len() == 8 && descriptor_count == 0 => Ok(Request::Retire {
            display: read_u64(payload, 0)?,
        }),
        opcode::CLIENT_HELLO
        | opcode::ACQUIRE
        | opcode::PRESENT
        | opcode::PRESENT_WITH_CONTENT
        | opcode::PRESENT_WITH_DAMAGE
        | opcode::CANCEL
        | opcode::RETIRE => Err(WireError::Length),
        _ => Err(WireError::Opcode),
    }
}

/// Encode one server response with its exact ancillary descriptor count.
///
/// # Errors
///
/// Returns [`WireError`] when the response fields or descriptor count violate
/// the protocol contract.
pub fn encode_response(response: &Response, descriptor_count: usize) -> Result<Vec<u8>, WireError> {
    let (opcode, payload) = match response {
        Response::Hello {
            display,
            width,
            height,
        } if *width != 0 && *height != 0 && descriptor_count == 0 => {
            let mut payload = Vec::with_capacity(16);
            push_u64(&mut payload, *display);
            push_u32(&mut payload, *width);
            push_u32(&mut payload, *height);
            (opcode::SERVER_HELLO, payload)
        }
        Response::Target {
            display,
            buffer,
            configure_serial,
            width,
            height,
            fourcc,
            modifier,
            planes,
        } if *buffer != 0
            && *configure_serial != 0
            && *width != 0
            && *height != 0
            && *fourcc != 0
            && !planes.is_empty()
            && planes.len() <= MAX_PLANES
            && descriptor_count == planes.len()
            && planes.iter().all(|plane| plane.stride != 0) =>
        {
            let mut payload = Vec::with_capacity(80);
            push_u64(&mut payload, *display);
            push_u64(&mut payload, *buffer);
            push_u32(&mut payload, *configure_serial);
            push_u32(&mut payload, *width);
            push_u32(&mut payload, *height);
            push_u32(&mut payload, *fourcc);
            push_u64(&mut payload, *modifier);
            push_u32(
                &mut payload,
                u32::try_from(planes.len()).map_err(|_| WireError::Field)?,
            );
            push_u32(&mut payload, 0);
            for index in 0..MAX_PLANES {
                push_u32(
                    &mut payload,
                    planes.get(index).map_or(0, |plane| plane.offset),
                );
            }
            for index in 0..MAX_PLANES {
                push_u32(
                    &mut payload,
                    planes.get(index).map_or(0, |plane| plane.stride),
                );
            }
            (opcode::TARGET, payload)
        }
        Response::Ack if descriptor_count == 0 => (opcode::ACK, Vec::new()),
        Response::Error { code } if descriptor_count == 0 => {
            let mut payload = Vec::with_capacity(4);
            payload.extend_from_slice(&code.to_le_bytes());
            (opcode::ERROR, payload)
        }
        _ => return Err(WireError::Descriptors),
    };
    encode(opcode, &payload, descriptor_count)
}

fn encode(opcode: u16, payload: &[u8], descriptor_count: usize) -> Result<Vec<u8>, WireError> {
    let total = HEADER_BYTES
        .checked_add(payload.len())
        .filter(|total| *total <= MAX_RECORD_BYTES)
        .ok_or(WireError::Length)?;
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&opcode.to_le_bytes());
    push_u32(
        &mut bytes,
        u32::try_from(payload.len()).map_err(|_| WireError::Length)?,
    );
    push_u32(
        &mut bytes,
        u32::try_from(descriptor_count).map_err(|_| WireError::Descriptors)?,
    );
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

fn decode_header(bytes: &[u8]) -> Result<(u16, &[u8], usize), WireError> {
    if bytes.len() < HEADER_BYTES || bytes.len() > MAX_RECORD_BYTES || bytes[..4] != MAGIC {
        return Err(WireError::Header);
    }
    if read_u16(bytes, 4)? != VERSION {
        return Err(WireError::Header);
    }
    let opcode = read_u16(bytes, 6)?;
    let payload_len = usize::try_from(read_u32(bytes, 8)?).map_err(|_| WireError::Length)?;
    if HEADER_BYTES.checked_add(payload_len) != Some(bytes.len()) {
        return Err(WireError::Length);
    }
    let descriptor_count =
        usize::try_from(read_u32(bytes, 12)?).map_err(|_| WireError::Descriptors)?;
    if descriptor_count > MAX_PLANES {
        return Err(WireError::Descriptors);
    }
    Ok((opcode, &bytes[HEADER_BYTES..], descriptor_count))
}

fn nonzero(value: u64) -> Result<u64, WireError> {
    (value != 0).then_some(value).ok_or(WireError::Field)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, WireError> {
    bytes
        .get(offset..offset + 2)
        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
        .map(u16::from_le_bytes)
        .ok_or(WireError::Length)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, WireError> {
    bytes
        .get(offset..offset + 4)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .map(u32::from_le_bytes)
        .ok_or(WireError::Length)
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, WireError> {
    bytes
        .get(offset..offset + 8)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map(u64::from_le_bytes)
        .ok_or(WireError::Length)
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(opcode: u16, payload: &[u8], descriptors: usize) -> Vec<u8> {
        encode(opcode, payload, descriptors).unwrap()
    }

    #[test]
    fn request_descriptor_contract_is_exact() {
        let mut payload = Vec::new();
        push_u64(&mut payload, 7);
        push_u64(&mut payload, 9);
        let bytes = request(opcode::PRESENT, &payload, 1);
        assert_eq!(
            decode_request(&bytes, 1).unwrap(),
            Request::Present {
                display: 7,
                buffer: 9,
                opaque: false,
                damage: None,
            }
        );
        assert_eq!(decode_request(&bytes, 0), Err(WireError::Descriptors));
        let synchronous = request(opcode::PRESENT, &payload, 0);
        assert!(matches!(
            decode_request(&synchronous, 0),
            Ok(Request::Present { .. })
        ));
    }

    #[test]
    fn opacity_is_frame_metadata_and_reserved_bits_are_rejected() {
        for (flags, opaque) in [(0_u32, false), (1_u32, true)] {
            let mut payload = Vec::new();
            push_u64(&mut payload, 7);
            push_u64(&mut payload, 9);
            push_u32(&mut payload, flags);
            push_u32(&mut payload, 0);
            for descriptors in [0, 1] {
                assert_eq!(decode_request(&request(opcode::PRESENT_WITH_CONTENT, &payload, descriptors), descriptors),
                    Ok(Request::Present { display: 7, buffer: 9, opaque, damage: None }));
            }
            payload[20] = 1;
            assert_eq!(decode_request(&request(opcode::PRESENT_WITH_CONTENT, &payload, 0), 0), Err(WireError::Field));
            payload[20] = 0;
            payload[16] = 2;
            assert_eq!(decode_request(&request(opcode::PRESENT_WITH_CONTENT, &payload, 0), 0), Err(WireError::Field));
            payload.truncate(16);
            assert_eq!(decode_request(&request(opcode::PRESENT_WITH_CONTENT, &payload, 0), 0), Err(WireError::Length));
        }
    }

    #[test]
    fn damage_preserves_partial_and_empty_frames_and_rejects_bad_records() {
        for (width, height) in [(100_u32, 80_u32), (0, 0)] {
            let mut payload = Vec::new();
            push_u64(&mut payload, 7);
            push_u64(&mut payload, 9);
            for value in [1, 0, 10, 20, width, height] {
                push_u32(&mut payload, value);
            }
            for descriptors in [0, 1] {
                assert_eq!(
                    decode_request(&request(opcode::PRESENT_WITH_DAMAGE, &payload, descriptors), descriptors),
                    Ok(Request::Present {
                        display: 7, buffer: 9, opaque: true,
                        damage: Some(DamageRect { x: 10, y: 20, width, height }),
                    })
                );
            }
            for offset in [16, 20] {
                let mut bad = payload.clone();
                bad[offset] = 2;
                assert_eq!(decode_request(&request(opcode::PRESENT_WITH_DAMAGE, &bad, 0), 0), Err(WireError::Field));
            }
            payload.pop();
            assert_eq!(decode_request(&request(opcode::PRESENT_WITH_DAMAGE, &payload, 0), 0), Err(WireError::Length));
        }
        for values in [[0, 0, 0, 1], [u32::MAX, 0, 1, 1], [0, u32::MAX, 1, 1]] {
            let mut payload = Vec::new();
            push_u64(&mut payload, 7);
            push_u64(&mut payload, 9);
            for value in [0, 0].into_iter().chain(values) { push_u32(&mut payload, value); }
            assert_eq!(decode_request(&request(opcode::PRESENT_WITH_DAMAGE, &payload, 0), 0), Err(WireError::Field));
        }
    }

    #[test]
    fn hello_selects_bootstrap_or_one_android_task() {
        let bootstrap = request(opcode::CLIENT_HELLO, &0_u64.to_le_bytes(), 0);
        assert_eq!(
            decode_request(&bootstrap, 0),
            Ok(Request::Hello { task: 0 })
        );
        let task = request(opcode::CLIENT_HELLO, &22_u64.to_le_bytes(), 0);
        assert_eq!(decode_request(&task, 0), Ok(Request::Hello { task: 22 }));
        assert_eq!(
            decode_request(&request(opcode::CLIENT_HELLO, &[], 0), 0),
            Err(WireError::Length)
        );
    }

    #[test]
    fn target_layout_is_fixed_and_bounded() {
        let response = Response::Target {
            display: 1,
            buffer: 2,
            configure_serial: 3,
            width: 800,
            height: 600,
            fourcc: u32::from_le_bytes(*b"AR24"),
            modifier: 0,
            planes: vec![Plane {
                offset: 0,
                stride: 3_200,
            }],
        };
        let encoded = encode_response(&response, 1).unwrap();
        assert_eq!(encoded.len(), 96);
        assert_eq!(u32::from_le_bytes(encoded[12..16].try_into().unwrap()), 1);
    }

    #[test]
    fn zero_buffer_is_rejected() {
        let mut payload = Vec::new();
        push_u64(&mut payload, 1);
        push_u64(&mut payload, 0);
        let bytes = request(opcode::CANCEL, &payload, 0);
        assert_eq!(decode_request(&bytes, 0), Err(WireError::Field));
    }

    #[test]
    fn task_retirement_names_its_selected_display() {
        let bytes = request(opcode::RETIRE, &7_u64.to_le_bytes(), 0);
        assert_eq!(
            decode_request(&bytes, 0),
            Ok(Request::Retire { display: 7 })
        );
        assert_eq!(
            decode_request(&request(opcode::RETIRE, &[], 0), 0),
            Err(WireError::Length)
        );
    }
}
