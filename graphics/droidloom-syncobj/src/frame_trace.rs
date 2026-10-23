//! Opt-in frame correlation and nonblocking GPU fence diagnostics.

pub use droidloom_timing::{Diagnostic, enabled, event, now_ns, sampled, traced};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, BorrowedFd};
const MAX_FENCES: usize = 64;

#[repr(C)]
#[derive(Default)]
struct FileInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    num_fences: u32,
    pad: u32,
    fences: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FenceInfo {
    object_name: [u8; 32],
    driver_name: [u8; 32],
    status: i32,
    flags: u32,
    timestamp_ns: u64,
}

/// Nonblocking SYNC_IOC_FILE_INFO snapshot. None means pending or no real timestamp.
/// The maximum component signal time is when a merged fence completes.
pub fn fence_signal_ns(fd: BorrowedFd<'_>) -> io::Result<Option<u64>> {
    const REQUEST: u64 =
        (3 << 30) | ((size_of::<FileInfo>() as u64) << 16) | ((b'>' as u64) << 8) | 4;
    let mut info = FileInfo::default();
    // SAFETY: Linux sync_file UAPI uses this exact repr(C) struct. With count zero
    // the kernel only returns the count; all pointers remain valid during ioctl.
    if unsafe { libc::ioctl(fd.as_raw_fd(), REQUEST as super::IoctlRequest, &mut info) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if info.status < 0 {
        return Err(io::Error::other("sync-file completed with an error"));
    }
    if info.status == 0 {
        return Ok(None);
    }
    if info.num_fences == 0 || info.num_fences as usize > MAX_FENCES {
        return Err(io::Error::other(
            "sync-file component count outside trace bound",
        ));
    }
    let mut fences = [FenceInfo {
        object_name: [0; 32],
        driver_name: [0; 32],
        status: 0,
        flags: 0,
        timestamp_ns: 0,
    }; MAX_FENCES];
    info.fences = fences.as_mut_ptr() as usize as u64;
    // SAFETY: the kernel writes at most the declared count, bounded by the live array.
    if unsafe { libc::ioctl(fd.as_raw_fd(), REQUEST as super::IoctlRequest, &mut info) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if info.num_fences as usize > MAX_FENCES {
        return Err(io::Error::other("sync-file count changed"));
    }
    summarize_fences(info.status, &fences[..info.num_fences as usize])
}

fn summarize_fences(status: i32, fences: &[FenceInfo]) -> io::Result<Option<u64>> {
    if status < 0 || fences.iter().any(|f| f.status < 0) {
        return Err(io::Error::other("sync-file component error"));
    }
    if status == 0
        || fences.is_empty()
        || fences.iter().any(|f| f.status == 0 || f.timestamp_ns == 0)
    {
        return Ok(None);
    }
    // DRM may substitute its global already-signalled stub when exporting a
    // completed timeline point. Its timestamp is from early boot, not this frame.
    let timestamp = fences
        .iter()
        .filter(|f| !f.driver_name.starts_with(b"stub\0"))
        .map(|f| f.timestamp_ns)
        .max();
    timestamp
        .map(Some)
        .ok_or_else(|| io::Error::other("placeholder fence has no GPU timestamp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fence(status: i32, timestamp_ns: u64) -> FenceInfo {
        FenceInfo {
            object_name: [0; 32],
            driver_name: [0; 32],
            status,
            flags: 0,
            timestamp_ns,
        }
    }
    #[test]
    fn merged_fence_uses_last_completion_and_never_invents_missing_time() {
        assert_eq!(
            summarize_fences(1, &[fence(1, 9), fence(1, 20)]).unwrap(),
            Some(20)
        );
        assert_eq!(
            summarize_fences(0, &[fence(1, 9), fence(0, 0)]).unwrap(),
            None
        );
        assert_eq!(summarize_fences(1, &[fence(1, 0)]).unwrap(), None);
        assert!(summarize_fences(-5, &[fence(-5, 20)]).is_err());
        let mut stub = fence(1, 999);
        stub.driver_name[..5].copy_from_slice(b"stub\0");
        assert!(summarize_fences(1, &[stub]).is_err());
        assert_eq!(
            summarize_fences(1, &[stub, fence(1, 20)]).unwrap(),
            Some(20)
        );
    }
    #[test]
    fn sync_file_uapi_layout() {
        assert_eq!(size_of::<FileInfo>(), 56);
        assert_eq!(size_of::<FenceInfo>(), 80);
    }
}
