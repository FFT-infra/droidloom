//! Unix transport for the Denial-native Droidloom protocol.
//!
//! Unsafe code is confined to the Linux/Android socket ABI in [`SeqPacket`].
//! Typed callers use [`ProtocolSocket`], which validates every packet with the
//! safe codec and binds each received descriptor to an exact protocol role.

#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::OsStr;
use std::io;
use std::mem::{self, MaybeUninit};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use droidloom_denial_protocol::{
    AndroidMessage, DecodedPacket, DenialMessage, DescriptorKind, MAX_PACKET_BYTES,
    MAX_PACKET_DESCRIPTORS, WireError, decode_android, decode_denial, encode_android,
    encode_denial,
};
use thiserror::Error;

/// Credentials authenticated by the kernel for a connected local peer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentials {
    /// Peer process ID at connection time.
    pub pid: u32,
    /// Effective peer user ID.
    pub uid: u32,
    /// Effective peer group ID.
    pub gid: u32,
}

/// One descriptor paired with the role validated from its packet.
#[derive(Debug)]
pub struct AttachedDescriptor {
    /// Message-defined descriptor role.
    pub kind: DescriptorKind,
    /// Owned descriptor received with close-on-exec set atomically.
    pub fd: OwnedFd,
}

/// One typed message and all of its role-bound descriptors.
#[derive(Debug)]
pub struct ReceivedMessage<T> {
    /// Decoded protocol message.
    pub message: T,
    /// Owned descriptors in the protocol-defined order.
    pub descriptors: Vec<AttachedDescriptor>,
}

/// Socket, ancillary-data, or wire failure.
#[derive(Debug, Error)]
pub enum IpcError {
    /// Linux/Android socket operation failed.
    #[error("socket I/O failed: {0}")]
    Io(#[from] io::Error),
    /// Endpoint path is empty, contains NUL, or exceeds `sockaddr_un`.
    #[error("invalid Unix socket path")]
    InvalidPath,
    /// A supplied descriptor is not a connected sequenced-packet socket.
    #[error("descriptor is not a Unix SOCK_SEQPACKET socket")]
    NotSeqPacket,
    /// Android init did not supply a valid named listening socket descriptor.
    #[error("invalid or missing Android init socket {0}")]
    InvalidInitSocket(String),
    /// A caller exceeded the protocol descriptor bound.
    #[error("packet carries {actual} descriptors; maximum is {maximum}")]
    DescriptorLimit {
        /// Supplied count.
        actual: usize,
        /// Protocol maximum.
        maximum: usize,
    },
    /// A typed packet was paired with the wrong descriptor count.
    #[error("message requires {expected} descriptors but caller supplied {actual}")]
    DescriptorMismatch {
        /// Count defined by the message.
        expected: usize,
        /// Count supplied by the caller.
        actual: usize,
    },
    /// Kernel reported record or ancillary truncation.
    #[error("sequenced packet or ancillary data was truncated")]
    Truncated,
    /// Ancillary data was malformed or had an unexpected type.
    #[error("invalid ancillary descriptor data")]
    InvalidAncillary,
    /// Sequenced-packet send did not consume exactly one complete record.
    #[error("short sequenced-packet send: {actual} of {expected} bytes")]
    ShortSend {
        /// Required record length.
        expected: usize,
        /// Bytes accepted by the kernel.
        actual: usize,
    },
    /// Peer performed an orderly socket shutdown.
    #[error("peer closed the protocol connection")]
    PeerClosed,
    /// Safe protocol codec rejected the record.
    #[error(transparent)]
    Wire(#[from] WireError),
}

/// Owned connected Unix sequenced-packet socket.
#[derive(Debug)]
pub struct SeqPacket {
    fd: OwnedFd,
}

/// Filesystem-backed Unix sequenced-packet listener.
///
/// The caller owns the containing mode-0700 runtime directory and socket
/// pathname. Binding never removes or replaces an existing entry, and dropping
/// the listener closes only its descriptor; the runtime transaction remains
/// responsible for removing the pathname.
#[derive(Debug)]
pub struct SeqPacketListener {
    fd: OwnedFd,
}

impl SeqPacketListener {
    /// Bind and listen at one private filesystem path.
    ///
    /// # Errors
    ///
    /// Rejects malformed paths and propagates socket, bind, or listen errors.
    /// An existing entry is never unlinked or replaced.
    pub fn bind(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        let (address, length) = unix_address(path.as_ref().as_os_str())?;
        // SAFETY: arguments are constants for a Unix sequenced-packet socket;
        // successful return transfers ownership of a fresh descriptor.
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: `socket` returned a new owned descriptor above.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: `address` is initialized for exactly `length` bytes and
        // remains live for the complete syscall.
        let result = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                ptr::from_ref(&address).cast::<libc::sockaddr>(),
                length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // A small bounded queue absorbs the service/client startup race. One
        // authenticated Android cell normally owns the only live connection.
        // SAFETY: `fd` is a live socket and backlog eight is a valid bound.
        if unsafe { libc::listen(fd.as_raw_fd(), 8) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self { fd })
    }

    /// Adopt an already listening Unix sequenced-packet descriptor.
    ///
    /// # Errors
    ///
    /// Rejects non-sockets, other socket types, and connected/non-listening
    /// descriptors.
    pub fn from_owned_fd(fd: OwnedFd) -> Result<Self, IpcError> {
        let socket = SeqPacket::from_owned_fd(fd)?;
        let mut accepting = 0_i32;
        let mut length = libc::socklen_t::try_from(mem::size_of_val(&accepting))
            .map_err(|_| IpcError::NotSeqPacket)?;
        // SAFETY: output points to a live integer of the advertised size.
        let result = unsafe {
            libc::getsockopt(
                socket.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ACCEPTCONN,
                ptr::from_mut(&mut accepting).cast(),
                &raw mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if accepting != 1 {
            return Err(IpcError::NotSeqPacket);
        }
        Ok(Self { fd: socket.fd })
    }

    /// Duplicate and adopt a named socket created by Android init's `socket`
    /// stanza. The original inherited descriptor remains owned by init/service
    /// startup; this object owns only its close-on-exec duplicate.
    ///
    /// # Errors
    ///
    /// Rejects malformed/missing `ANDROID_SOCKET_<name>` state and descriptors
    /// that are not listening Unix sequenced-packet sockets.
    pub fn from_android_init_socket(name: &str) -> Result<Self, IpcError> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(IpcError::InvalidInitSocket(name.to_owned()));
        }
        let variable = android_init_descriptor_variable(name);
        let raw = std::env::var(&variable)
            .ok()
            .and_then(|value| value.parse::<RawFd>().ok())
            .filter(|fd| *fd >= 0)
            .ok_or_else(|| IpcError::InvalidInitSocket(name.to_owned()))?;
        // SAFETY: `F_DUPFD_CLOEXEC` does not consume `raw`; on success it
        // returns a distinct owned descriptor with close-on-exec set.
        let duplicate = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful `F_DUPFD_CLOEXEC` transferred ownership of the
        // new descriptor to this call.
        let fd = unsafe { OwnedFd::from_raw_fd(duplicate) };
        Self::from_owned_fd(fd)
    }

    /// Accept one connected endpoint with close-on-exec set atomically.
    ///
    /// # Errors
    ///
    /// Propagates `accept4(2)` failures, including `WouldBlock` for a
    /// non-blocking listener with no pending peer.
    pub fn accept(&self) -> Result<SeqPacket, IpcError> {
        // SAFETY: null address pointers explicitly decline peer-address output;
        // successful return transfers ownership of a fresh connected socket.
        let raw = unsafe {
            libc::accept4(
                self.fd.as_raw_fd(),
                ptr::null_mut(),
                ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful `accept4` returned a new owned descriptor.
        Ok(SeqPacket {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        })
    }

    /// Enable or disable non-blocking accept for event-loop integration.
    ///
    /// # Errors
    ///
    /// Propagates `fcntl(2)` failures.
    pub fn set_nonblocking(&self, enabled: bool) -> Result<(), IpcError> {
        // SAFETY: `F_GETFL` reads flags from the live owned descriptor.
        let current = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GETFL) };
        if current < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let updated = if enabled {
            current | libc::O_NONBLOCK
        } else {
            current & !libc::O_NONBLOCK
        };
        // SAFETY: `updated` changes only file-status flags on the live socket.
        if unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFL, updated) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
}

fn android_init_descriptor_variable(name: &str) -> String {
    format!("ANDROID_SOCKET_{name}")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

impl AsFd for SeqPacketListener {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl SeqPacket {
    /// Connect to an existing filesystem-backed Unix sequenced-packet socket.
    ///
    /// The resulting descriptor is close-on-exec. Authentication remains the
    /// caller's responsibility through [`Self::peer_credentials`] and the
    /// supervisor's runtime-directory policy.
    ///
    /// # Errors
    ///
    /// Rejects malformed paths and propagates socket/connect failures.
    pub fn connect(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        let (address, length) = unix_address(path.as_ref().as_os_str())?;
        // SAFETY: arguments are constants for a Unix sequenced-packet socket;
        // successful return transfers ownership of a fresh descriptor.
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: `socket` returned a new owned descriptor above.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: `address` is initialized with the exact validated length and
        // remains alive for the duration of the syscall.
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                ptr::from_ref(&address).cast::<libc::sockaddr>(),
                length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self { fd })
    }

    /// Construct a connected in-process pair, primarily for endpoint tests.
    ///
    /// # Errors
    ///
    /// Propagates `socketpair(2)` failure.
    pub fn pair() -> Result<(Self, Self), IpcError> {
        let mut raw = [-1; 2];
        // SAFETY: `raw` has space for both returned descriptors. On success the
        // kernel initializes both and transfers their ownership to this call.
        let result = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                raw.as_mut_ptr(),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful `socketpair` returned two distinct owned FDs.
        let left = unsafe { OwnedFd::from_raw_fd(raw[0]) };
        // SAFETY: successful `socketpair` returned two distinct owned FDs.
        let right = unsafe { OwnedFd::from_raw_fd(raw[1]) };
        Ok((Self { fd: left }, Self { fd: right }))
    }

    /// Adopt an already connected descriptor after validating its socket type.
    ///
    /// # Errors
    ///
    /// Rejects non-sockets and socket types other than `SOCK_SEQPACKET`.
    pub fn from_owned_fd(fd: OwnedFd) -> Result<Self, IpcError> {
        let mut socket_type = 0_i32;
        let mut length = libc::socklen_t::try_from(mem::size_of_val(&socket_type))
            .map_err(|_| IpcError::NotSeqPacket)?;
        // SAFETY: the output points to a live `i32` and its exact size. The
        // descriptor remains owned for the complete call.
        let result = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                ptr::from_mut(&mut socket_type).cast(),
                &raw mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if socket_type != libc::SOCK_SEQPACKET {
            return Err(IpcError::NotSeqPacket);
        }
        Ok(Self { fd })
    }

    /// Enable or disable non-blocking operation for event-loop integration.
    ///
    /// # Errors
    ///
    /// Propagates `fcntl(2)` failures.
    pub fn set_nonblocking(&self, enabled: bool) -> Result<(), IpcError> {
        // SAFETY: `F_GETFL` reads flags from the live owned descriptor.
        let current = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GETFL) };
        if current < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let updated = if enabled {
            current | libc::O_NONBLOCK
        } else {
            current & !libc::O_NONBLOCK
        };
        // SAFETY: `updated` changes only file-status flags on the live socket.
        if unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFL, updated) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    /// Read kernel-authenticated peer PID, UID, and GID.
    ///
    /// # Errors
    ///
    /// Propagates `SO_PEERCRED` failure or a malformed kernel response.
    pub fn peer_credentials(&self) -> Result<PeerCredentials, IpcError> {
        let mut credentials = MaybeUninit::<libc::ucred>::uninit();
        let mut length = libc::socklen_t::try_from(mem::size_of::<libc::ucred>())
            .map_err(|_| IpcError::InvalidAncillary)?;
        // SAFETY: the kernel writes at most the advertised `ucred` size into
        // valid uninitialized storage, and the descriptor stays live.
        let result = unsafe {
            libc::getsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &raw mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if usize::try_from(length).ok() != Some(mem::size_of::<libc::ucred>()) {
            return Err(IpcError::InvalidAncillary);
        }
        // SAFETY: successful `getsockopt` initialized the complete `ucred`.
        let credentials = unsafe { credentials.assume_init() };
        Ok(PeerCredentials {
            pid: u32::try_from(credentials.pid).map_err(|_| IpcError::InvalidAncillary)?,
            uid: credentials.uid,
            gid: credentials.gid,
        })
    }

    /// Send exactly one record with up to four attached descriptors.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized records and descriptor overflow, then propagates
    /// `sendmsg(2)` failure or an impossible short sequenced-packet send.
    #[allow(
        clippy::cast_ptr_alignment,
        reason = "CMSG_DATA is aligned for the SCM_RIGHTS RawFd array by the kernel ABI"
    )]
    pub fn send_record(
        &self,
        record: &[u8],
        descriptors: &[BorrowedFd<'_>],
    ) -> Result<(), IpcError> {
        if record.is_empty() || record.len() > MAX_PACKET_BYTES {
            return Err(IpcError::Truncated);
        }
        if descriptors.len() > MAX_PACKET_DESCRIPTORS {
            return Err(IpcError::DescriptorLimit {
                actual: descriptors.len(),
                maximum: MAX_PACKET_DESCRIPTORS,
            });
        }

        let mut iovec = libc::iovec {
            iov_base: record.as_ptr().cast_mut().cast(),
            iov_len: record.len(),
        };
        let mut control = AlignedControl::default();
        // SAFETY: zero is a valid empty `msghdr`; every pointer/length used by
        // the syscall is initialized below and lives through the call.
        let mut message = unsafe { mem::zeroed::<libc::msghdr>() };
        message.msg_iov = &raw mut iovec;
        message.msg_iovlen = 1;

        if !descriptors.is_empty() {
            message.msg_control = control.bytes.as_mut_ptr().cast();
            message.msg_controllen = control.bytes.len();
            // SAFETY: the aligned control region is writable and large enough
            // for the version-1 maximum. `CMSG_FIRSTHDR` returns its first
            // header for this initialized `msghdr`.
            let header = unsafe { libc::CMSG_FIRSTHDR(&raw const message) };
            if header.is_null() {
                return Err(IpcError::InvalidAncillary);
            }
            let descriptor_bytes = descriptors
                .len()
                .checked_mul(mem::size_of::<RawFd>())
                .ok_or(IpcError::InvalidAncillary)?;
            let descriptor_bytes_u32 =
                u32::try_from(descriptor_bytes).map_err(|_| IpcError::InvalidAncillary)?;
            // SAFETY: `header` points inside `control`; the macros compute the
            // ABI-defined header/data sizes for `descriptor_bytes`.
            unsafe {
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(descriptor_bytes_u32) as usize;
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                for (index, descriptor) in descriptors.iter().enumerate() {
                    data.add(index).write(descriptor.as_raw_fd());
                }
                message.msg_controllen = libc::CMSG_SPACE(descriptor_bytes_u32) as usize;
            }
        }

        // SAFETY: the message references live record/control storage and only
        // borrowed FDs. The kernel does not retain these pointers after return.
        let sent =
            unsafe { libc::sendmsg(self.fd.as_raw_fd(), &raw const message, libc::MSG_NOSIGNAL) };
        if sent < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let sent = usize::try_from(sent).map_err(|_| IpcError::ShortSend {
            expected: record.len(),
            actual: 0,
        })?;
        if sent != record.len() {
            return Err(IpcError::ShortSend {
                expected: record.len(),
                actual: sent,
            });
        }
        Ok(())
    }

    /// Receive exactly one complete record and all attached descriptors.
    ///
    /// Received FDs have close-on-exec set by `MSG_CMSG_CLOEXEC`. Any error
    /// after descriptor extraction drops every extracted FD before returning.
    ///
    /// # Errors
    ///
    /// Rejects truncation, malformed ancillary data, descriptor overflow, and
    /// peer shutdown; otherwise propagates `recvmsg(2)` failures.
    pub fn receive_record(&self) -> Result<RawRecord, IpcError> {
        let mut bytes = vec![0_u8; MAX_PACKET_BYTES];
        let record = self.receive_record_into(&mut bytes)?;
        let length = record.bytes.len();
        let descriptors = record.descriptors;
        bytes.truncate(length);
        Ok(RawRecord { bytes, descriptors })
    }

    /// Receive into caller-owned storage, borrowing only the initialized record.
    /// Reusing this storage avoids allocating or clearing a maximum-sized packet
    /// on each frame/input event or unsuccessful nonblocking receive.
    ///
    /// # Errors
    /// Rejects empty or oversized storage and truncated packets. Ancillary
    /// descriptors are closed on failure, including payload truncation.
    pub fn receive_record_into<'a>(
        &self,
        bytes: &'a mut [u8],
    ) -> Result<BorrowedRecord<'a>, IpcError> {
        if bytes.is_empty() || bytes.len() > MAX_PACKET_BYTES {
            return Err(IpcError::Truncated);
        }
        let mut iovec = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut control = AlignedControl::default();
        // SAFETY: zero is a valid empty `msghdr`; all buffer pointers and
        // lengths are initialized below and remain live through `recvmsg`.
        let mut message = unsafe { mem::zeroed::<libc::msghdr>() };
        message.msg_iov = &raw mut iovec;
        message.msg_iovlen = 1;
        message.msg_control = control.bytes.as_mut_ptr().cast();
        message.msg_controllen = control.bytes.len();

        // SAFETY: the writable data/control regions exactly match the lengths
        // in `message`. `MSG_CMSG_CLOEXEC` atomically protects received FDs.
        let received = unsafe {
            libc::recvmsg(
                self.fd.as_raw_fd(),
                &raw mut message,
                libc::MSG_CMSG_CLOEXEC,
            )
        };
        if received < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if received == 0 {
            return Err(IpcError::PeerClosed);
        }

        let mut descriptors = extract_descriptors(&message)?;
        if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
            descriptors.clear();
            return Err(IpcError::Truncated);
        }
        if descriptors.len() > MAX_PACKET_DESCRIPTORS {
            let actual = descriptors.len();
            descriptors.clear();
            return Err(IpcError::DescriptorLimit {
                actual,
                maximum: MAX_PACKET_DESCRIPTORS,
            });
        }
        let received = usize::try_from(received).map_err(|_| IpcError::Truncated)?;
        Ok(BorrowedRecord {
            bytes: &bytes[..received],
            descriptors,
        })
    }
}

impl AsFd for SeqPacket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Untyped record returned by [`SeqPacket::receive_record`].
#[derive(Debug)]
pub struct RawRecord {
    /// Complete record bytes.
    pub bytes: Vec<u8>,
    /// Unclassified owned ancillary descriptors.
    pub descriptors: Vec<OwnedFd>,
}

/// A received record borrowing caller-owned payload storage.
#[derive(Debug)]
pub struct BorrowedRecord<'a> {
    /// Exactly the received bytes; unused receive capacity is never exposed.
    pub bytes: &'a [u8],
    /// Owned ancillary descriptors, closed when dropped.
    pub descriptors: Vec<OwnedFd>,
}

/// Protocol-aware wrapper around one connected sequenced-packet socket.
#[derive(Debug)]
pub struct ProtocolSocket {
    socket: SeqPacket,
}

impl ProtocolSocket {
    /// Wrap a validated connected socket.
    pub fn new(socket: SeqPacket) -> Self {
        Self { socket }
    }

    /// Borrow the underlying socket for event-loop registration and peer auth.
    pub fn socket(&self) -> &SeqPacket {
        &self.socket
    }

    /// Send one Android-to-Denial message with its exact descriptors.
    ///
    /// # Errors
    ///
    /// Rejects invalid messages and descriptor-count mismatches before a
    /// syscall, then propagates transport errors.
    pub fn send_android(
        &self,
        message: &AndroidMessage,
        descriptors: &[BorrowedFd<'_>],
    ) -> Result<(), IpcError> {
        let encoded = encode_android(message)?;
        require_descriptor_count(encoded.descriptors.len(), descriptors.len())?;
        self.socket.send_record(&encoded.bytes, descriptors)
    }

    /// Receive and validate one Android-to-Denial message.
    ///
    /// # Errors
    ///
    /// Propagates record and codec failures. All FDs close on failure.
    pub fn receive_android(&self) -> Result<ReceivedMessage<AndroidMessage>, IpcError> {
        let record = self.socket.receive_record()?;
        attach_decoded(
            decode_android(&record.bytes, record.descriptors.len())?,
            record.descriptors,
        )
    }

    /// Receive an Android message using reusable payload storage.
    ///
    /// # Errors
    /// Propagates transport/codec failures and closes all FDs on failure.
    pub fn receive_android_into(
        &self,
        bytes: &mut [u8],
    ) -> Result<ReceivedMessage<AndroidMessage>, IpcError> {
        let record = self.socket.receive_record_into(bytes)?;
        attach_decoded(
            decode_android(record.bytes, record.descriptors.len())?,
            record.descriptors,
        )
    }

    /// Send one Denial-to-Android message.
    ///
    /// # Errors
    ///
    /// Rejects invalid messages before a syscall and propagates transport
    /// failures.
    pub fn send_denial(&self, message: &DenialMessage) -> Result<(), IpcError> {
        self.send_denial_with_descriptors(message, &[])
    }

    /// Send one Denial-to-Android message with its codec-defined descriptors.
    ///
    /// # Errors
    ///
    /// Rejects a descriptor count that differs from the typed message layout
    /// and propagates transport failures.
    pub fn send_denial_with_descriptors(
        &self,
        message: &DenialMessage,
        descriptors: &[BorrowedFd<'_>],
    ) -> Result<(), IpcError> {
        let encoded = encode_denial(message)?;
        require_descriptor_count(encoded.descriptors.len(), descriptors.len())?;
        self.socket.send_record(&encoded.bytes, descriptors)
    }

    /// Receive and validate one Denial-to-Android message.
    ///
    /// # Errors
    ///
    /// Propagates record and codec failures. Unexpected FDs close on failure.
    pub fn receive_denial(&self) -> Result<ReceivedMessage<DenialMessage>, IpcError> {
        let record = self.socket.receive_record()?;
        attach_decoded(
            decode_denial(&record.bytes, record.descriptors.len())?,
            record.descriptors,
        )
    }

    /// Receive a host message using reusable payload storage.
    ///
    /// # Errors
    /// Propagates transport/codec failures and closes all FDs on failure.
    pub fn receive_denial_into(
        &self,
        bytes: &mut [u8],
    ) -> Result<ReceivedMessage<DenialMessage>, IpcError> {
        let record = self.socket.receive_record_into(bytes)?;
        attach_decoded(
            decode_denial(record.bytes, record.descriptors.len())?,
            record.descriptors,
        )
    }
}

#[repr(C, align(16))]
struct AlignedControl {
    bytes: [u8; 128],
}

impl Default for AlignedControl {
    fn default() -> Self {
        Self { bytes: [0; 128] }
    }
}

fn unix_address(path: &OsStr) -> Result<(libc::sockaddr_un, libc::socklen_t), IpcError> {
    let bytes = path.as_bytes();
    // SAFETY: all-zero is a valid initial representation of `sockaddr_un`.
    let mut address = unsafe { mem::zeroed::<libc::sockaddr_un>() };
    if bytes.is_empty()
        || bytes.contains(&0)
        || bytes.len().saturating_add(1) > address.sun_path.len()
    {
        return Err(IpcError::InvalidPath);
    }
    address.sun_family =
        libc::sa_family_t::try_from(libc::AF_UNIX).map_err(|_| IpcError::InvalidPath)?;
    // SAFETY: `c_char` and `u8` are both one byte; the slice is limited to the
    // validated path length within `sun_path`.
    let destination = unsafe {
        std::slice::from_raw_parts_mut(address.sun_path.as_mut_ptr().cast::<u8>(), bytes.len())
    };
    destination.copy_from_slice(bytes);
    let length = mem::offset_of!(libc::sockaddr_un, sun_path)
        .checked_add(bytes.len())
        .and_then(|value| value.checked_add(1))
        .and_then(|value| libc::socklen_t::try_from(value).ok())
        .ok_or(IpcError::InvalidPath)?;
    Ok((address, length))
}

#[allow(
    clippy::cast_ptr_alignment,
    reason = "CMSG_DATA is aligned for the SCM_RIGHTS RawFd array by the kernel ABI"
)]
fn extract_descriptors(message: &libc::msghdr) -> Result<Vec<OwnedFd>, IpcError> {
    let mut descriptors = Vec::new();
    // SAFETY: `message` was populated by `recvmsg`, and its aligned control
    // buffer remains live for this function. The libc macros honor its bounds.
    let mut header = unsafe { libc::CMSG_FIRSTHDR(ptr::from_ref(message)) };
    while !header.is_null() {
        // SAFETY: a non-null header returned by the CMSG macros points inside
        // the received control region.
        let current = unsafe { &*header };
        if current.cmsg_level != libc::SOL_SOCKET || current.cmsg_type != libc::SCM_RIGHTS {
            return Err(IpcError::InvalidAncillary);
        }
        // SAFETY: a zero-length payload asks only for the ABI header size.
        let base = unsafe { libc::CMSG_LEN(0) as usize };
        if current.cmsg_len < base {
            return Err(IpcError::InvalidAncillary);
        }
        let data_bytes = current.cmsg_len - base;
        if data_bytes == 0 || data_bytes % mem::size_of::<RawFd>() != 0 {
            return Err(IpcError::InvalidAncillary);
        }
        let count = data_bytes / mem::size_of::<RawFd>();
        // SAFETY: SCM_RIGHTS payload length was validated as a whole number of
        // `RawFd` values and `CMSG_DATA` supplies their aligned start.
        let data = unsafe { libc::CMSG_DATA(header).cast::<RawFd>() };
        for index in 0..count {
            // SAFETY: `index` is within the validated SCM_RIGHTS payload.
            let raw = unsafe { data.add(index).read() };
            if raw < 0 {
                return Err(IpcError::InvalidAncillary);
            }
            // SAFETY: `recvmsg` installed a new descriptor owned by this
            // process. `MSG_CMSG_CLOEXEC` already marked it close-on-exec.
            descriptors.push(unsafe { OwnedFd::from_raw_fd(raw) });
        }
        // SAFETY: the libc macro advances within the same live control region.
        header = unsafe { libc::CMSG_NXTHDR(ptr::from_ref(message), header) };
    }
    Ok(descriptors)
}

fn require_descriptor_count(expected: usize, actual: usize) -> Result<(), IpcError> {
    if expected == actual {
        Ok(())
    } else {
        Err(IpcError::DescriptorMismatch { expected, actual })
    }
}

fn attach_decoded<T>(
    decoded: DecodedPacket<T>,
    descriptors: Vec<OwnedFd>,
) -> Result<ReceivedMessage<T>, IpcError> {
    require_descriptor_count(decoded.descriptors.len(), descriptors.len())?;
    let descriptors = decoded
        .descriptors
        .into_iter()
        .zip(descriptors)
        .map(|(kind, fd)| AttachedDescriptor { kind, fd })
        .collect();
    Ok(ReceivedMessage {
        message: decoded.message,
        descriptors,
    })
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;

    use droidloom_denial_protocol::{
        AndroidMessage, BufferId, BufferMetadata, FormatModifier, PlaneMetadata, TaskObjectId,
        capability,
    };

    use super::*;

    #[test]
    fn android_init_socket_name_matches_descriptor_sanitization() {
        assert_eq!(
            android_init_descriptor_variable("droidloom-task-control"),
            "ANDROID_SOCKET_droidloom_task_control"
        );
        assert_eq!(
            android_init_descriptor_variable("already_valid"),
            "ANDROID_SOCKET_already_valid"
        );
    }

    #[test]
    fn sequenced_socket_preserves_record_boundaries() {
        let (left, right) = SeqPacket::pair().unwrap();
        left.send_record(b"first", &[]).unwrap();
        left.send_record(b"second", &[]).unwrap();
        assert_eq!(right.receive_record().unwrap().bytes, b"first");
        assert_eq!(right.receive_record().unwrap().bytes, b"second");
    }

    #[test]
    fn reusable_storage_exposes_only_each_record_and_survives_would_block() {
        let (left, right) = SeqPacket::pair().unwrap();
        right.set_nonblocking(true).unwrap();
        let mut bytes = [0xaa; 128];
        let storage = bytes.as_ptr();
        for payload in [b"longer first record".as_slice(), b"next", b"x"] {
            left.send_record(payload, &[]).unwrap();
            let record = right.receive_record_into(&mut bytes).unwrap();
            assert_eq!(record.bytes.as_ptr(), storage);
            assert_eq!(record.bytes, payload);
            assert!(record.descriptors.is_empty());
            assert!(matches!(right.receive_record_into(&mut bytes),
                Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock));
        }
    }

    #[test]
    fn truncated_reusable_record_closes_received_descriptors() {
        let (left, right) = SeqPacket::pair().unwrap();
        let (mut observer, transferred) = std::os::unix::net::UnixStream::pair().unwrap();
        observer.set_nonblocking(true).unwrap();
        left.send_record(&[42; 129], &[transferred.as_fd()])
            .unwrap();
        drop(transferred);
        assert!(matches!(
            right.receive_record_into(&mut [0; 128]),
            Err(IpcError::Truncated)
        ));
        // EOF, rather than WouldBlock, proves that the received peer FD closed.
        assert_eq!(observer.read(&mut [0; 1]).unwrap(), 0);
        left.send_record(b"next", &[]).unwrap();
        assert_eq!(
            right.receive_record_into(&mut [0; 128]).unwrap().bytes,
            b"next"
        );
    }

    #[test]
    fn reusable_typed_receiver_preserves_both_message_directions() {
        let (left, right) = SeqPacket::pair().unwrap();
        let android = ProtocolSocket::new(left);
        let host = ProtocolSocket::new(right);
        let mut bytes = [0; 128];
        let message = AndroidMessage::Pong { cookie: 42 };
        android.send_android(&message, &[]).unwrap();
        assert_eq!(
            host.receive_android_into(&mut bytes).unwrap().message,
            message
        );
        let message = DenialMessage::Ping { cookie: 43 };
        host.send_denial(&message).unwrap();
        assert_eq!(
            android.receive_denial_into(&mut bytes).unwrap().message,
            message
        );
    }

    #[test]
    #[ignore = "local socket microbenchmark; run explicitly with --ignored --nocapture"]
    fn benchmark_reused_record_storage() {
        use std::hint::black_box;
        use std::time::Instant;
        let (left, right) = SeqPacket::pair().unwrap();
        let payload = [42; 112];
        let mut bytes = [0; 128];
        const RECORDS: u32 = 50_000;
        // Alternate order to reduce warmup/order bias. This measures only local
        // send/receive and buffer management, not graphics or input latency.
        for trial in 0..6 {
            for reused in if trial % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let start = Instant::now();
                for _ in 0..RECORDS {
                    left.send_record(black_box(&payload), &[]).unwrap();
                    if reused {
                        black_box(right.receive_record_into(&mut bytes).unwrap());
                    } else {
                        black_box(right.receive_record().unwrap());
                    }
                }
                eprintln!(
                    "trial={trial} reused={reused} ns_per_record={}",
                    start.elapsed().as_nanos() / u128::from(RECORDS)
                );
            }
        }
    }

    #[test]
    fn filesystem_listener_accepts_without_losing_record_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("denial-droidloom.sock");
        let listener = match SeqPacketListener::bind(&path) {
            Ok(listener) => listener,
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied => {
                // Some test sandboxes deny filesystem Unix-socket binds. CI
                // and the focused native ingress test exercise the full path.
                return;
            }
            Err(error) => panic!("unexpected listener bind failure: {error}"),
        };
        listener.set_nonblocking(true).unwrap();
        assert!(matches!(
            listener.accept(),
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock
        ));

        let client = SeqPacket::connect(&path).unwrap();
        let server = listener.accept().unwrap();
        client.send_record(b"hello", &[]).unwrap();
        client.send_record(b"frame", &[]).unwrap();
        assert_eq!(server.receive_record().unwrap().bytes, b"hello");
        assert_eq!(server.receive_record().unwrap().bytes, b"frame");
    }

    #[test]
    fn listener_never_replaces_an_existing_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owned-by-someone-else");
        File::create(&path).unwrap();

        assert!(matches!(
            SeqPacketListener::bind(&path),
            Err(IpcError::Io(_))
        ));
        assert!(path.is_file());
    }

    #[test]
    fn inherited_listener_descriptor_is_validated_before_accept() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("inherited.sock");
        let original = match SeqPacketListener::bind(&path) {
            Ok(listener) => listener,
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("unexpected listener bind failure: {error}"),
        };
        let inherited = original.as_fd().try_clone_to_owned().unwrap();
        let listener = SeqPacketListener::from_owned_fd(inherited).unwrap();
        drop(original);

        let client = SeqPacket::connect(&path).unwrap();
        let server = listener.accept().unwrap();
        client.send_record(b"inherited", &[]).unwrap();
        assert_eq!(server.receive_record().unwrap().bytes, b"inherited");
    }

    #[test]
    fn protocol_socket_binds_dma_buf_to_the_validated_plane_role() {
        let (android, denial) = SeqPacket::pair().unwrap();
        let android = ProtocolSocket::new(android);
        let denial = ProtocolSocket::new(denial);
        let plane = File::open("/dev/null").unwrap();
        let message = AndroidMessage::RegisterBuffer {
            object: TaskObjectId(7),
            buffer: BufferMetadata {
                id: BufferId(1),
                width: 64,
                height: 64,
                format: FormatModifier {
                    fourcc: u32::from_le_bytes(*b"XR24"),
                    modifier: 0,
                },
                planes: vec![PlaneMetadata {
                    index: 0,
                    offset: 0,
                    stride: 256,
                }],
            },
        };
        android.send_android(&message, &[plane.as_fd()]).unwrap();
        let mut received = denial.receive_android().unwrap();
        assert_eq!(received.message, message);
        assert_eq!(received.descriptors.len(), 1);
        assert_eq!(received.descriptors[0].kind, DescriptorKind::DmabufPlane);
        let mut imported = File::from(received.descriptors.remove(0).fd);
        let mut byte = [0_u8; 1];
        assert_eq!(imported.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn descriptor_mismatch_fails_before_sending() {
        let (android, denial) = SeqPacket::pair().unwrap();
        let android = ProtocolSocket::new(android);
        denial.set_nonblocking(true).unwrap();
        let message = AndroidMessage::BindTimelines {
            object: TaskObjectId(7),
        };
        assert!(matches!(
            android.send_android(&message, &[]),
            Err(IpcError::DescriptorMismatch {
                expected: 2,
                actual: 0,
            })
        ));
        assert!(matches!(
            denial.receive_record(),
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn handshake_and_peer_credentials_round_trip() {
        let (android, denial) = SeqPacket::pair().unwrap();
        match denial.peer_credentials() {
            Ok(credentials) => {
                let process = std::fs::metadata("/proc/self").unwrap();
                assert_eq!(credentials.uid, process.uid());
                assert_eq!(credentials.gid, process.gid());
                assert_eq!(credentials.pid, std::process::id());
            }
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied => {
                // Some test sandboxes block SO_PEERCRED. The handshake and all
                // record/FD behavior remain testable in that environment.
            }
            Err(error) => panic!("unexpected peer-credential failure: {error}"),
        }

        let android = ProtocolSocket::new(android);
        let denial = ProtocolSocket::new(denial);
        let hello = AndroidMessage::ClientHello {
            min_major: 1,
            max_major: 1,
            capabilities: capability::REQUIRED_V1,
        };
        android.send_android(&hello, &[]).unwrap();
        assert_eq!(denial.receive_android().unwrap().message, hello);
    }
}
