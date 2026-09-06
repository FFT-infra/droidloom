//! Safe parser for the pinned minigbm `cros_gralloc_handle` payload.
//!
//! Binder transports leading descriptors separately from the handle's
//! remaining 32-bit words. This crate reconstructs bounded metadata only;
//! descriptor ownership stays with the Android-specific adapter.

#![forbid(unsafe_code)]

use droidloom_transport::{
    BufferId, BufferMetadata, FormatModifier, PlaneMetadata, TransportError,
};
use thiserror::Error;

/// Number of 32-bit payload words following `native_handle_t` in the pinned
/// minigbm `cros_gralloc_handle` ABI.
pub const CROS_GRALLOC_HANDLE_WORDS: usize = 36;
/// Maximum image planes carried by the pinned handle.
pub const CROS_GRALLOC_MAX_PLANES: usize = 4;
/// Maximum descriptors: four planes and one optional reserved-region FD.
pub const CROS_GRALLOC_MAX_FDS: usize = CROS_GRALLOC_MAX_PLANES + 1;
/// `cros_gralloc_handle::magic` at the pinned minigbm commit.
pub const CROS_GRALLOC_MAGIC: u32 = 0xabcd_dcba;
/// Android protected-buffer usage bit.
pub const ANDROID_USAGE_PROTECTED: u64 = 1 << 14;

const STRIDES_WORD: usize = 5;
const OFFSETS_WORD: usize = 9;
const SIZES_WORD: usize = 13;
const ID_WORD: usize = 17;
const WIDTH_WORD: usize = 18;
const HEIGHT_WORD: usize = 19;
const FORMAT_WORD: usize = 20;
const TILING_WORD: usize = 21;
const MODIFIER_WORD: usize = 22;
const USE_FLAGS_WORD: usize = 24;
const MAGIC_WORD: usize = 26;
const PIXEL_STRIDE_WORD: usize = 27;
const DROID_FORMAT_WORD: usize = 28;
const USAGE_WORD: usize = 29;
const NUM_PLANES_WORD: usize = 31;
const RESERVED_REGION_SIZE_WORD: usize = 32;
const TOTAL_SIZE_WORD: usize = 34;

/// Metadata reconstructed from one validated minigbm native handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MinigbmMetadata {
    /// Protocol-independent DMA-BUF metadata consumed by the Composer core.
    pub buffer: BufferMetadata,
    /// Number of leading Binder descriptors that represent image planes.
    pub plane_fd_count: usize,
    /// Whether the final descriptor owns minigbm's reserved metadata region.
    pub has_reserved_region_fd: bool,
    /// Android producer/consumer usage flags.
    pub android_usage: u64,
    /// Minigbm internal allocation flags.
    pub use_flags: u64,
    /// Android pixel-format discriminator retained for diagnostics.
    pub droid_format: i32,
    /// Minigbm tiling discriminator retained for diagnostics.
    pub tiling: u32,
    /// Android pixel stride.
    pub pixel_stride: u32,
    /// Size of the optional reserved metadata region.
    pub reserved_region_size: u64,
    /// Total allocation size reported by minigbm.
    pub total_size: u64,
}

impl MinigbmMetadata {
    /// Whether this allocation requests end-to-end protected presentation.
    pub fn is_protected(&self) -> bool {
        self.android_usage & ANDROID_USAGE_PROTECTED != 0
    }
}

/// Malformed or incompatible minigbm handle payload.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MinigbmError {
    /// Binder did not preserve the fixed 36-word native-handle payload.
    #[error("minigbm handle has {actual} payload words, expected {expected}")]
    PayloadWords {
        /// Required fixed payload size.
        expected: usize,
        /// Descriptors plus integer words received from Binder.
        actual: usize,
    },
    /// Descriptor count is outside the fixed ABI bound.
    #[error("minigbm handle has invalid descriptor count {0}")]
    DescriptorCount(usize),
    /// A requested absolute ABI word was absent.
    #[error("minigbm handle is missing ABI word {0}")]
    MissingWord(usize),
    /// The payload does not carry the pinned minigbm magic value.
    #[error("minigbm handle has invalid magic {0:#010x}")]
    Magic(u32),
    /// Plane count is outside the supported one-through-four range.
    #[error("minigbm handle has invalid plane count {0}")]
    PlaneCount(u32),
    /// Plane and optional reserved-region descriptors do not match metadata.
    #[error("minigbm handle has {actual} descriptors, expected {expected}")]
    DescriptorPlaneMismatch {
        /// Plane descriptors plus an optional reserved-region descriptor.
        expected: usize,
        /// Binder descriptor count.
        actual: usize,
    },
    /// DRM `FourCC` is zero and cannot describe a DMA-BUF.
    #[error("minigbm handle has no DRM FourCC")]
    MissingFormat,
    /// The implicit/invalid modifier sentinel is forbidden by the explicit path.
    #[error("minigbm handle uses DRM_FORMAT_MOD_INVALID")]
    InvalidModifier,
    /// A plane has no storage.
    #[error("minigbm plane {0} has zero size")]
    EmptyPlane(usize),
    /// A plane range exceeds the allocation size or overflows.
    #[error("minigbm plane {0} exceeds the reported allocation size")]
    PlaneOutOfBounds(usize),
    /// Total allocation size is zero.
    #[error("minigbm handle reports a zero-size allocation")]
    EmptyAllocation,
    /// Generic bounded DMA-BUF metadata validation failed.
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// Parse one Binder-split `cros_gralloc_handle` without reading or taking
/// ownership of any descriptor.
///
/// `fd_count` is the length of `NativeHandle.fds`; `ints` is
/// `NativeHandle.ints`. Used descriptors occupy the beginning of the fixed
/// handle, so unused entries in minigbm's five-FD array appear at the beginning
/// of `ints`.
///
/// # Errors
///
/// Rejects any payload that is not the exact pinned ABI, has inconsistent
/// descriptor/plane counts, or cannot describe a bounded explicit-modifier
/// DMA-BUF.
pub fn parse_minigbm_handle(
    fd_count: usize,
    ints: &[i32],
) -> Result<MinigbmMetadata, MinigbmError> {
    if fd_count == 0 || fd_count > CROS_GRALLOC_MAX_FDS {
        return Err(MinigbmError::DescriptorCount(fd_count));
    }
    let actual_words = fd_count
        .checked_add(ints.len())
        .ok_or(MinigbmError::PayloadWords {
            expected: CROS_GRALLOC_HANDLE_WORDS,
            actual: usize::MAX,
        })?;
    if actual_words != CROS_GRALLOC_HANDLE_WORDS {
        return Err(MinigbmError::PayloadWords {
            expected: CROS_GRALLOC_HANDLE_WORDS,
            actual: actual_words,
        });
    }

    let words = SplitWords { fd_count, ints };
    let magic = words.u32(MAGIC_WORD)?;
    if magic != CROS_GRALLOC_MAGIC {
        return Err(MinigbmError::Magic(magic));
    }

    let raw_plane_count = words.u32(NUM_PLANES_WORD)?;
    let plane_count = usize::try_from(raw_plane_count)
        .ok()
        .filter(|count| (1..=CROS_GRALLOC_MAX_PLANES).contains(count))
        .ok_or(MinigbmError::PlaneCount(raw_plane_count))?;
    let reserved_region_size = words.u64(RESERVED_REGION_SIZE_WORD)?;
    let has_reserved_region_fd = reserved_region_size != 0;
    let expected_fds = plane_count + usize::from(has_reserved_region_fd);
    if fd_count != expected_fds {
        return Err(MinigbmError::DescriptorPlaneMismatch {
            expected: expected_fds,
            actual: fd_count,
        });
    }

    let format = words.u32(FORMAT_WORD)?;
    if format == 0 {
        return Err(MinigbmError::MissingFormat);
    }
    let modifier = words.u64(MODIFIER_WORD)?;
    if modifier == u64::MAX {
        return Err(MinigbmError::InvalidModifier);
    }
    let total_size = words.u64(TOTAL_SIZE_WORD)?;
    if total_size == 0 {
        return Err(MinigbmError::EmptyAllocation);
    }

    let mut planes = Vec::with_capacity(plane_count);
    for index in 0..plane_count {
        let stride = words.u32(STRIDES_WORD + index)?;
        let offset = words.u32(OFFSETS_WORD + index)?;
        let size = words.u32(SIZES_WORD + index)?;
        if size == 0 {
            return Err(MinigbmError::EmptyPlane(index));
        }
        let end = u64::from(offset)
            .checked_add(u64::from(size))
            .ok_or(MinigbmError::PlaneOutOfBounds(index))?;
        if end > total_size {
            return Err(MinigbmError::PlaneOutOfBounds(index));
        }
        planes.push(PlaneMetadata {
            index: u32::try_from(index).map_err(|_| MinigbmError::PlaneCount(u32::MAX))?,
            offset,
            stride,
        });
    }

    let buffer = BufferMetadata {
        id: BufferId(u64::from(words.u32(ID_WORD)?)),
        width: words.u32(WIDTH_WORD)?,
        height: words.u32(HEIGHT_WORD)?,
        format: FormatModifier {
            fourcc: format,
            modifier,
        },
        planes,
    };
    buffer.validate()?;

    Ok(MinigbmMetadata {
        buffer,
        plane_fd_count: plane_count,
        has_reserved_region_fd,
        android_usage: words.u64(USAGE_WORD)?,
        use_flags: words.u64(USE_FLAGS_WORD)?,
        droid_format: i32::from_ne_bytes(words.u32(DROID_FORMAT_WORD)?.to_ne_bytes()),
        tiling: words.u32(TILING_WORD)?,
        pixel_stride: words.u32(PIXEL_STRIDE_WORD)?,
        reserved_region_size,
        total_size,
    })
}

struct SplitWords<'a> {
    fd_count: usize,
    ints: &'a [i32],
}

impl SplitWords<'_> {
    fn u32(&self, absolute: usize) -> Result<u32, MinigbmError> {
        let index = absolute
            .checked_sub(self.fd_count)
            .ok_or(MinigbmError::MissingWord(absolute))?;
        self.ints
            .get(index)
            .copied()
            .map(|word| u32::from_ne_bytes(word.to_ne_bytes()))
            .ok_or(MinigbmError::MissingWord(absolute))
    }

    fn u64(&self, absolute: usize) -> Result<u64, MinigbmError> {
        Ok(u64::from(self.u32(absolute)?) | (u64::from(self.u32(absolute + 1)?) << 32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;

    fn valid_words(planes: usize, reserved_region_size: u64) -> [u32; CROS_GRALLOC_HANDLE_WORDS] {
        let mut words = [0_u32; CROS_GRALLOC_HANDLE_WORDS];
        let plane_size = 1280 * 4 * 720;
        for index in 0..planes {
            words[STRIDES_WORD + index] = 1280 * 4;
            words[SIZES_WORD + index] = plane_size;
        }
        words[ID_WORD] = 17;
        words[WIDTH_WORD] = 1280;
        words[HEIGHT_WORD] = 720;
        words[FORMAT_WORD] = DRM_FORMAT_ARGB8888;
        words[MAGIC_WORD] = CROS_GRALLOC_MAGIC;
        words[PIXEL_STRIDE_WORD] = 1280;
        words[DROID_FORMAT_WORD] = 1;
        words[NUM_PLANES_WORD] = u32::try_from(planes).unwrap();
        words[RESERVED_REGION_SIZE_WORD] =
            u32::try_from(reserved_region_size & u64::from(u32::MAX)).unwrap();
        words[RESERVED_REGION_SIZE_WORD + 1] = u32::try_from(reserved_region_size >> 32).unwrap();
        words[TOTAL_SIZE_WORD] = plane_size;
        words
    }

    fn split(words: &[u32; CROS_GRALLOC_HANDLE_WORDS], fd_count: usize) -> Vec<i32> {
        words[fd_count..]
            .iter()
            .map(|word| i32::from_ne_bytes(word.to_ne_bytes()))
            .collect()
    }

    #[test]
    fn parses_one_plane_handle_after_binder_fd_split() {
        let words = valid_words(1, 0);
        let parsed = parse_minigbm_handle(1, &split(&words, 1)).unwrap();

        assert_eq!(parsed.plane_fd_count, 1);
        assert!(!parsed.has_reserved_region_fd);
        assert_eq!(parsed.buffer.id, BufferId(17));
        assert_eq!(parsed.buffer.width, 1280);
        assert_eq!(parsed.buffer.height, 720);
        assert_eq!(parsed.buffer.format.fourcc, DRM_FORMAT_ARGB8888);
        assert_eq!(parsed.buffer.planes[0].stride, 5120);
    }

    #[test]
    fn accepts_optional_reserved_region_descriptor() {
        let words = valid_words(1, 4096);
        let parsed = parse_minigbm_handle(2, &split(&words, 2)).unwrap();

        assert!(parsed.has_reserved_region_fd);
        assert_eq!(parsed.reserved_region_size, 4096);
        assert_eq!(parsed.plane_fd_count, 1);
    }

    #[test]
    fn parses_all_four_plane_descriptors() {
        let words = valid_words(4, 0);
        let parsed = parse_minigbm_handle(4, &split(&words, 4)).unwrap();

        assert_eq!(parsed.buffer.planes.len(), 4);
        assert_eq!(parsed.plane_fd_count, 4);
    }

    #[test]
    fn extracts_protected_usage_without_guessing_format() {
        let mut words = valid_words(1, 0);
        words[USAGE_WORD] = u32::try_from(ANDROID_USAGE_PROTECTED).unwrap();
        let parsed = parse_minigbm_handle(1, &split(&words, 1)).unwrap();

        assert!(parsed.is_protected());
    }

    #[test]
    fn rejects_payload_length_drift() {
        let words = valid_words(1, 0);
        let mut ints = split(&words, 1);
        ints.pop();

        assert_eq!(
            parse_minigbm_handle(1, &ints),
            Err(MinigbmError::PayloadWords {
                expected: CROS_GRALLOC_HANDLE_WORDS,
                actual: CROS_GRALLOC_HANDLE_WORDS - 1,
            })
        );
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut words = valid_words(1, 0);
        words[MAGIC_WORD] = 0;

        assert_eq!(
            parse_minigbm_handle(1, &split(&words, 1)),
            Err(MinigbmError::Magic(0))
        );
    }

    #[test]
    fn rejects_inconsistent_reserved_region_descriptor() {
        let words = valid_words(1, 4096);

        assert_eq!(
            parse_minigbm_handle(1, &split(&words, 1)),
            Err(MinigbmError::DescriptorPlaneMismatch {
                expected: 2,
                actual: 1,
            })
        );
    }

    #[test]
    fn rejects_implicit_modifier_sentinel() {
        let mut words = valid_words(1, 0);
        words[MODIFIER_WORD] = u32::MAX;
        words[MODIFIER_WORD + 1] = u32::MAX;

        assert_eq!(
            parse_minigbm_handle(1, &split(&words, 1)),
            Err(MinigbmError::InvalidModifier)
        );
    }

    #[test]
    fn rejects_plane_beyond_total_allocation() {
        let mut words = valid_words(1, 0);
        words[OFFSETS_WORD] = 1;

        assert_eq!(
            parse_minigbm_handle(1, &split(&words, 1)),
            Err(MinigbmError::PlaneOutOfBounds(0))
        );
    }
}
