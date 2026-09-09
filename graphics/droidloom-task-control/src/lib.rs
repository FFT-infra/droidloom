//! Versioned control records for reserving one native window per Android task.
//!
//! This is a privileged lifecycle channel, separate from the high-frequency
//! DMA-BUF presentation protocol. Direct tasks are registered only after the
//! launcher or privileged task observer resolves their `ActivityTaskManager` identity, making the
//! registry the allowlist for windows `SurfaceFlinger` may export to the host.

#![forbid(unsafe_code)]

use thiserror::Error;

/// Protocol marker at the start of each sequenced-packet record.
pub const MAGIC: [u8; 4] = *b"DLTC";
/// Supported protocol major.
pub const PROTOCOL_MAJOR: u16 = 3;
/// Supported protocol minor.
pub const PROTOCOL_MINOR: u16 = 0;
/// Fixed bytes before the opcode-specific payload.
pub const HEADER_BYTES: usize = 24;
/// Maximum complete task-control record.
pub const MAX_RECORD_BYTES: usize = 4_096;
/// Maximum Android package identity length.
pub const MAX_PACKAGE_BYTES: usize = 255;
/// Maximum bounded diagnostic text.
pub const MAX_ERROR_BYTES: usize = 1_024;

const OP_HELLO: u16 = 0x0001;
const OP_RESERVE: u16 = 0x0002;
const OP_BIND: u16 = 0x0003;
const OP_REMOVE: u16 = 0x0004;
const OP_REGISTER_DIRECT: u16 = 0x0005;
const OP_SERVER_HELLO: u16 = 0x8001;
const OP_RESERVED: u16 = 0x8002;
const OP_BOUND: u16 = 0x8003;
const OP_REMOVED: u16 = 0x8004;
const OP_ERROR: u16 = 0x80ff;

/// Privileged controller request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlRequest {
    /// Negotiate the connection before lifecycle requests.
    Hello {
        /// Oldest supported major.
        min_major: u16,
        /// Newest supported major.
        max_major: u16,
    },
    /// Reserve and configure one native task window before Android launch.
    Reserve {
        /// Nonzero client correlation identity.
        request_id: u64,
        /// Package the controller intends to launch.
        package: String,
    },
    /// Bind the real task identity assigned after launch.
    Bind {
        /// Nonzero client correlation identity.
        request_id: u64,
        /// Object returned by [`ControlResponse::Reserved`].
        object: u64,
        /// Real `ActivityTaskManager` task ID.
        task: u64,
        /// Logical display ID assigned by Android's display manager.
        android_display: u32,
    },
    /// Authorize one real task on Android's built-in display for direct
    /// `SurfaceFlinger` composition into a host window.
    RegisterDirect {
        /// Nonzero client correlation identity.
        request_id: u64,
        /// Verified package owning the task.
        package: String,
        /// Real `ActivityTaskManager` task ID.
        task: u64,
        /// Android display receiving input for this task.
        android_display: u32,
    },
    /// Tear down a reserved or bound task window.
    Remove {
        /// Nonzero client correlation identity.
        request_id: u64,
        /// Object returned by [`ControlResponse::Reserved`].
        object: u64,
    },
}

/// Task-control service response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlResponse {
    /// Selected protocol version.
    ServerHello {
        /// Selected major.
        major: u16,
        /// Selected minor.
        minor: u16,
    },
    /// Denial configured the window and `SurfaceFlinger` received hotplug.
    Reserved {
        /// Matching request identity.
        request_id: u64,
        /// Monotonic Denial object identity.
        object: u64,
        /// Stable Composer display handle; Android assigns a separate logical
        /// display ID after processing the asynchronous hotplug.
        display: u64,
    },
    /// The real Android task identity was bound end to end.
    Bound {
        /// Matching request identity.
        request_id: u64,
        /// Bound Denial object.
        object: u64,
        /// Bound Android task.
        task: u64,
        /// Stable Composer display handle associated with the bound task.
        display: u64,
        /// Logical display ID targeted by Android input events.
        android_display: u32,
    },
    /// The native task window was removed.
    Removed {
        /// Matching request identity.
        request_id: u64,
        /// Removed Denial object.
        object: u64,
    },
    /// Bounded request failure; the connection remains usable only when the
    /// service can prove its state is unambiguous.
    Error {
        /// Matching request identity, or zero for connection errors.
        request_id: u64,
        /// Stable machine-readable error code.
        code: ControlErrorCode,
        /// Bounded UTF-8 diagnostic.
        message: String,
    },
}

/// Stable failure code returned by the control service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ControlErrorCode {
    /// Version negotiation failed or was omitted.
    Protocol = 1,
    /// Package/object/task input was invalid.
    InvalidRequest = 2,
    /// A requested identity was not live.
    NotFound = 3,
    /// An identity was already bound or otherwise conflicted.
    Conflict = 4,
    /// Denial did not configure the task within the bounded deadline.
    ConfigureTimeout = 5,
    /// Composer, Denial, or `SurfaceFlinger` integration failed.
    Backend = 6,
}

impl TryFrom<u16> for ControlErrorCode {
    type Error = CodecError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Protocol),
            2 => Ok(Self::InvalidRequest),
            3 => Ok(Self::NotFound),
            4 => Ok(Self::Conflict),
            5 => Ok(Self::ConfigureTimeout),
            6 => Ok(Self::Backend),
            _ => Err(CodecError::InvalidValue("control error code")),
        }
    }
}

/// Malformed or incompatible control record.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CodecError {
    /// Record was shorter than its header or declared payload.
    #[error("truncated task-control record")]
    Truncated,
    /// Record marker was not [`MAGIC`].
    #[error("invalid task-control magic")]
    InvalidMagic,
    /// Record major is unsupported.
    #[error("unsupported task-control major {0}")]
    UnsupportedMajor(u16),
    /// Opcode is unknown for this direction.
    #[error("unknown task-control opcode {0:#06x}")]
    UnknownOpcode(u16),
    /// Header length or reserved fields were inconsistent.
    #[error("invalid task-control header")]
    InvalidHeader,
    /// Field was zero, malformed, or outside its fixed bound.
    #[error("invalid task-control {0}")]
    InvalidValue(&'static str),
    /// Text payload was not UTF-8.
    #[error("task-control text is not UTF-8")]
    InvalidUtf8,
}

/// Encode one controller request.
///
/// # Errors
///
/// Rejects invalid version ranges, identities, packages, and record overflow.
pub fn encode_request(request: &ControlRequest) -> Result<Vec<u8>, CodecError> {
    let mut payload = Writer::default();
    let (opcode, request_id) = match request {
        ControlRequest::Hello {
            min_major,
            max_major,
        } => {
            if *min_major == 0 || min_major > max_major {
                return Err(CodecError::InvalidValue("version range"));
            }
            payload.u16(*min_major);
            payload.u16(*max_major);
            (OP_HELLO, 0)
        }
        ControlRequest::Reserve {
            request_id,
            package,
        } => {
            nonzero("request identity", *request_id)?;
            if !valid_package(package) {
                return Err(CodecError::InvalidValue("package"));
            }
            payload.string(package, MAX_PACKAGE_BYTES)?;
            (OP_RESERVE, *request_id)
        }
        ControlRequest::Bind {
            request_id,
            object,
            task,
            android_display,
        } => {
            nonzero("request identity", *request_id)?;
            nonzero("object identity", *object)?;
            nonzero("task identity", *task)?;
            payload.u64(*object);
            payload.u64(*task);
            payload.u32(*android_display);
            (OP_BIND, *request_id)
        }
        ControlRequest::RegisterDirect {
            request_id,
            package,
            task,
            android_display,
        } => {
            nonzero("request identity", *request_id)?;
            nonzero("task identity", *task)?;
            if !valid_package(package) {
                return Err(CodecError::InvalidValue("package"));
            }
            payload.string(package, MAX_PACKAGE_BYTES)?;
            payload.u64(*task);
            payload.u32(*android_display);
            (OP_REGISTER_DIRECT, *request_id)
        }
        ControlRequest::Remove { request_id, object } => {
            nonzero("request identity", *request_id)?;
            nonzero("object identity", *object)?;
            payload.u64(*object);
            (OP_REMOVE, *request_id)
        }
    };
    encode(opcode, request_id, &payload.bytes)
}

/// Decode one controller request.
///
/// # Errors
///
/// Rejects malformed, oversized, incompatible, or wrong-direction records.
pub fn decode_request(record: &[u8]) -> Result<ControlRequest, CodecError> {
    let header = decode_header(record)?;
    let mut payload = Reader::new(header.payload);
    let request = match header.opcode {
        OP_HELLO => {
            if header.request_id != 0 {
                return Err(CodecError::InvalidHeader);
            }
            let min_major = payload.u16()?;
            let max_major = payload.u16()?;
            if min_major == 0 || min_major > max_major {
                return Err(CodecError::InvalidValue("version range"));
            }
            ControlRequest::Hello {
                min_major,
                max_major,
            }
        }
        OP_RESERVE => {
            nonzero("request identity", header.request_id)?;
            let package = payload.string(MAX_PACKAGE_BYTES)?;
            if !valid_package(&package) {
                return Err(CodecError::InvalidValue("package"));
            }
            ControlRequest::Reserve {
                request_id: header.request_id,
                package,
            }
        }
        OP_BIND => ControlRequest::Bind {
            request_id: required_request(header.request_id)?,
            object: required("object identity", payload.u64()?)?,
            task: required("task identity", payload.u64()?)?,
            android_display: payload.u32()?,
        },
        OP_REGISTER_DIRECT => {
            let request_id = required_request(header.request_id)?;
            let package = payload.string(MAX_PACKAGE_BYTES)?;
            if !valid_package(&package) {
                return Err(CodecError::InvalidValue("package"));
            }
            ControlRequest::RegisterDirect {
                request_id,
                package,
                task: required("task identity", payload.u64()?)?,
                android_display: payload.u32()?,
            }
        }
        OP_REMOVE => ControlRequest::Remove {
            request_id: required_request(header.request_id)?,
            object: required("object identity", payload.u64()?)?,
        },
        opcode => return Err(CodecError::UnknownOpcode(opcode)),
    };
    payload.finish()?;
    Ok(request)
}

/// Encode one service response.
///
/// # Errors
///
/// Rejects invalid identities, diagnostics, versions, and record overflow.
pub fn encode_response(response: &ControlResponse) -> Result<Vec<u8>, CodecError> {
    let mut payload = Writer::default();
    let (opcode, request_id) = match response {
        ControlResponse::ServerHello { major, minor } => {
            if *major == 0 {
                return Err(CodecError::InvalidValue("selected version"));
            }
            payload.u16(*major);
            payload.u16(*minor);
            (OP_SERVER_HELLO, 0)
        }
        ControlResponse::Reserved {
            request_id,
            object,
            display,
        } => {
            require_response_ids(*request_id, &[*object, *display])?;
            payload.u64(*object);
            payload.u64(*display);
            (OP_RESERVED, *request_id)
        }
        ControlResponse::Bound {
            request_id,
            object,
            task,
            display,
            android_display,
        } => {
            require_response_ids(*request_id, &[*object, *task, *display])?;
            payload.u64(*object);
            payload.u64(*task);
            payload.u64(*display);
            payload.u32(*android_display);
            (OP_BOUND, *request_id)
        }
        ControlResponse::Removed { request_id, object } => {
            require_response_ids(*request_id, &[*object])?;
            payload.u64(*object);
            (OP_REMOVED, *request_id)
        }
        ControlResponse::Error {
            request_id,
            code,
            message,
        } => {
            if message.is_empty() || message.len() > MAX_ERROR_BYTES {
                return Err(CodecError::InvalidValue("error text"));
            }
            payload.u16(*code as u16);
            payload.string(message, MAX_ERROR_BYTES)?;
            (OP_ERROR, *request_id)
        }
    };
    encode(opcode, request_id, &payload.bytes)
}

/// Decode one service response.
///
/// # Errors
///
/// Rejects malformed, oversized, incompatible, or wrong-direction records.
pub fn decode_response(record: &[u8]) -> Result<ControlResponse, CodecError> {
    let header = decode_header(record)?;
    let mut payload = Reader::new(header.payload);
    let response = match header.opcode {
        OP_SERVER_HELLO => {
            if header.request_id != 0 {
                return Err(CodecError::InvalidHeader);
            }
            let major = payload.u16()?;
            if major == 0 {
                return Err(CodecError::InvalidValue("selected major"));
            }
            ControlResponse::ServerHello {
                major,
                minor: payload.u16()?,
            }
        }
        OP_RESERVED => ControlResponse::Reserved {
            request_id: required_request(header.request_id)?,
            object: required("object identity", payload.u64()?)?,
            display: required("display identity", payload.u64()?)?,
        },
        OP_BOUND => ControlResponse::Bound {
            request_id: required_request(header.request_id)?,
            object: required("object identity", payload.u64()?)?,
            task: required("task identity", payload.u64()?)?,
            display: required("display identity", payload.u64()?)?,
            android_display: payload.u32()?,
        },
        OP_REMOVED => ControlResponse::Removed {
            request_id: required_request(header.request_id)?,
            object: required("object identity", payload.u64()?)?,
        },
        OP_ERROR => ControlResponse::Error {
            request_id: header.request_id,
            code: ControlErrorCode::try_from(payload.u16()?)?,
            message: payload.string(MAX_ERROR_BYTES)?,
        },
        opcode => return Err(CodecError::UnknownOpcode(opcode)),
    };
    payload.finish()?;
    Ok(response)
}

fn encode(opcode: u16, request_id: u64, payload: &[u8]) -> Result<Vec<u8>, CodecError> {
    let total = HEADER_BYTES
        .checked_add(payload.len())
        .ok_or(CodecError::InvalidHeader)?;
    if total > MAX_RECORD_BYTES {
        return Err(CodecError::InvalidHeader);
    }
    let payload_len = u32::try_from(payload.len()).map_err(|_| CodecError::InvalidHeader)?;
    let mut record = Vec::with_capacity(total);
    record.extend_from_slice(&MAGIC);
    record.extend_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record.extend_from_slice(&PROTOCOL_MINOR.to_le_bytes());
    record.extend_from_slice(&opcode.to_le_bytes());
    record.extend_from_slice(&0_u16.to_le_bytes());
    record.extend_from_slice(&payload_len.to_le_bytes());
    record.extend_from_slice(&request_id.to_le_bytes());
    record.extend_from_slice(payload);
    Ok(record)
}

struct Header<'a> {
    opcode: u16,
    request_id: u64,
    payload: &'a [u8],
}

fn decode_header(record: &[u8]) -> Result<Header<'_>, CodecError> {
    if record.len() < HEADER_BYTES || record.len() > MAX_RECORD_BYTES {
        return Err(CodecError::Truncated);
    }
    if record[..4] != MAGIC {
        return Err(CodecError::InvalidMagic);
    }
    let major = u16::from_le_bytes(record[4..6].try_into().map_err(|_| CodecError::Truncated)?);
    if major != PROTOCOL_MAJOR {
        return Err(CodecError::UnsupportedMajor(major));
    }
    let reserved = u16::from_le_bytes(
        record[10..12]
            .try_into()
            .map_err(|_| CodecError::Truncated)?,
    );
    let payload_len = u32::from_le_bytes(
        record[12..16]
            .try_into()
            .map_err(|_| CodecError::Truncated)?,
    ) as usize;
    if reserved != 0 || HEADER_BYTES.checked_add(payload_len) != Some(record.len()) {
        return Err(CodecError::InvalidHeader);
    }
    Ok(Header {
        opcode: u16::from_le_bytes(
            record[8..10]
                .try_into()
                .map_err(|_| CodecError::Truncated)?,
        ),
        request_id: u64::from_le_bytes(
            record[16..24]
                .try_into()
                .map_err(|_| CodecError::Truncated)?,
        ),
        payload: &record[HEADER_BYTES..],
    })
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str, maximum: usize) -> Result<(), CodecError> {
        if value.is_empty() || value.len() > maximum {
            return Err(CodecError::InvalidValue("text"));
        }
        let length = u16::try_from(value.len()).map_err(|_| CodecError::InvalidValue("text"))?;
        self.u16(length);
        self.bytes.extend_from_slice(value.as_bytes());
        Ok(())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], CodecError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or(CodecError::Truncated)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(CodecError::Truncated)?;
        self.offset = end;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_le_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| CodecError::Truncated)?,
        ))
    }

    fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_le_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| CodecError::Truncated)?,
        ))
    }

    fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| CodecError::Truncated)?,
        ))
    }

    fn string(&mut self, maximum: usize) -> Result<String, CodecError> {
        let length = usize::from(self.u16()?);
        if length == 0 || length > maximum {
            return Err(CodecError::InvalidValue("text"));
        }
        let bytes = self.take(length)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| CodecError::InvalidUtf8)
    }

    fn finish(self) -> Result<(), CodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(CodecError::InvalidHeader)
        }
    }
}

fn nonzero(field: &'static str, value: u64) -> Result<(), CodecError> {
    required(field, value).map(|_| ())
}

fn required(field: &'static str, value: u64) -> Result<u64, CodecError> {
    if value == 0 {
        Err(CodecError::InvalidValue(field))
    } else {
        Ok(value)
    }
}

fn required_request(value: u64) -> Result<u64, CodecError> {
    required("request identity", value)
}

fn require_response_ids(request_id: u64, identities: &[u64]) -> Result<(), CodecError> {
    required_request(request_id)?;
    if identities.contains(&0) {
        return Err(CodecError::InvalidValue("response identity"));
    }
    Ok(())
}

fn valid_package(package: &str) -> bool {
    !package.is_empty()
        && package.len() <= MAX_PACKAGE_BYTES
        && package.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_phase_launch_records_round_trip() {
        for request in [
            ControlRequest::Hello {
                min_major: 1,
                max_major: 1,
            },
            ControlRequest::Reserve {
                request_id: 1,
                package: "org.example.app".to_owned(),
            },
            ControlRequest::Bind {
                request_id: 2,
                object: 7,
                task: 42,
                android_display: 3,
            },
            ControlRequest::RegisterDirect {
                request_id: 3,
                package: "org.example.app".to_owned(),
                task: 42,
                android_display: 0,
            },
            ControlRequest::Remove {
                request_id: 4,
                object: 7,
            },
        ] {
            assert_eq!(
                decode_request(&encode_request(&request).unwrap()).unwrap(),
                request
            );
        }

        for response in [
            ControlResponse::ServerHello { major: 3, minor: 0 },
            ControlResponse::Reserved {
                request_id: 1,
                object: 7,
                display: 9,
            },
            ControlResponse::Bound {
                request_id: 2,
                object: 7,
                task: 42,
                display: 9,
                android_display: 3,
            },
            ControlResponse::Removed {
                request_id: 3,
                object: 7,
            },
            ControlResponse::Error {
                request_id: 4,
                code: ControlErrorCode::Backend,
                message: "composer unavailable".to_owned(),
            },
        ] {
            assert_eq!(
                decode_response(&encode_response(&response).unwrap()).unwrap(),
                response
            );
        }
    }

    #[test]
    fn malformed_ids_packages_and_framing_fail_closed() {
        assert!(matches!(
            encode_request(&ControlRequest::Reserve {
                request_id: 1,
                package: "not/a/package".to_owned(),
            }),
            Err(CodecError::InvalidValue("package"))
        ));
        assert!(
            encode_request(&ControlRequest::Bind {
                request_id: 1,
                object: 0,
                task: 42,
                android_display: 3,
            })
            .is_err()
        );

        let mut record = encode_request(&ControlRequest::Remove {
            request_id: 1,
            object: 7,
        })
        .unwrap();
        record.push(0);
        assert_eq!(decode_request(&record), Err(CodecError::InvalidHeader));
    }

    #[test]
    fn request_and_response_directions_are_distinct() {
        let response = encode_response(&ControlResponse::Reserved {
            request_id: 1,
            object: 2,
            display: 3,
        })
        .unwrap();
        assert_eq!(
            decode_request(&response),
            Err(CodecError::UnknownOpcode(OP_RESERVED))
        );
    }
}
