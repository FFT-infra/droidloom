//! DRM syncobj timeline bridge for the Denial-native presentation path.
//!
//! Android creates one acquire and one release timeline per task and exports
//! their opaque descriptors to Denial once. Per-frame acquire and completion
//! fences are then transferred into points on those shared timelines without
//! sending a descriptor with every `Present` record.
//!
//! Unsafe code is confined to the one Linux DRM ioctl which `drm-rs` does not
//! expose correctly for importing a `sync_file` into an *existing* syncobj.

#![deny(unsafe_op_in_unsafe_fn)]

use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use drm::control::{Device as ControlDevice, syncobj};
use thiserror::Error;

const DRM_IOCTL_BASE: u64 = b'd' as u64;
const DRM_SYNCOBJ_FD_TO_HANDLE_NR: u64 = 0xc2;
const DRM_SYNCOBJ_IMPORT_SYNC_FILE: u32 = 1;
const IOC_NR_SHIFT: u32 = 0;
const IOC_TYPE_SHIFT: u32 = 8;
const IOC_SIZE_SHIFT: u32 = 16;
const IOC_DIR_SHIFT: u32 = 30;
const IOC_READ_WRITE: u64 = 3;

#[cfg(target_os = "android")]
type IoctlRequest = libc::c_int;
#[cfg(not(target_os = "android"))]
type IoctlRequest = libc::c_ulong;

/// Operation did not complete before the supplied absolute monotonic deadline.
pub const NO_WAIT: i64 = 0;
/// Effectively unbounded absolute monotonic deadline for a kernel syncobj wait.
pub const WAIT_FOREVER: i64 = i64::MAX;

/// Syncobj creation, import, transfer, wait, or export failure.
#[derive(Debug, Error)]
pub enum SyncobjError {
    /// Timeline point zero is reserved and cannot identify a frame.
    #[error("DRM syncobj timeline point must be non-zero")]
    ZeroPoint,
    /// A task attempted to reuse or reorder a point on one timeline.
    #[error("DRM syncobj timeline point {attempted} is not newer than {previous}")]
    PointRegression {
        /// Last point committed to this operation stream.
        previous: u64,
        /// Reused or older point supplied by the caller.
        attempted: u64,
    },
    /// A Linux DRM operation failed.
    #[error("{operation}: {source}")]
    Drm {
        /// Stable operation name for diagnostics and evidence.
        operation: &'static str,
        /// Kernel error.
        #[source]
        source: io::Error,
    },
    /// The monotonic clock could not be read for a bounded availability wait.
    #[error("read monotonic clock: {0}")]
    Clock(#[source] io::Error),
    /// Current monotonic time plus the requested timeout exceeds the DRM ABI.
    #[error("DRM syncobj wait deadline exceeds signed nanosecond range")]
    DeadlineOverflow,
}

/// Convert a relative duration into the absolute monotonic nanosecond deadline
/// required by `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT`.
///
/// # Errors
///
/// Propagates `clock_gettime(2)` failure and rejects arithmetic overflow.
pub fn monotonic_deadline_after(timeout: Duration) -> Result<i64, SyncobjError> {
    let mut now = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `now` points to writable storage of exactly one `timespec`; a
    // successful syscall initializes it completely.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, now.as_mut_ptr()) } != 0 {
        return Err(SyncobjError::Clock(io::Error::last_os_error()));
    }
    // SAFETY: successful `clock_gettime` above initialized the value.
    let now = unsafe { now.assume_init() };
    deadline_from_parts(now.tv_sec, now.tv_nsec, timeout)
}

/// Cloneable access to one already-open DRM render node.
///
/// The caller chooses and opens the node. Droidloom does not discover or open
/// a KMS card node here; Android should receive only its selected render node.
#[derive(Clone, Debug)]
pub struct SyncobjDevice(Arc<File>);

impl SyncobjDevice {
    /// Adopt an already-open render-node file.
    pub fn from_file(file: File) -> Self {
        Self(Arc::new(file))
    }

    /// Adopt an already-open render-node descriptor.
    pub fn from_owned_fd(fd: OwnedFd) -> Self {
        Self::from_file(File::from(fd))
    }

    fn create_timeline(&self) -> Result<Timeline, SyncobjError> {
        let handle = self
            .create_syncobj(false)
            .map_err(|source| drm_error("create timeline syncobj", source))?;
        Ok(Timeline {
            device: self.clone(),
            handle: Some(handle),
            last_attached_point: 0,
            last_signalled_point: 0,
            last_exported_point: 0,
        })
    }

    fn import_timeline(&self, fd: BorrowedFd<'_>) -> Result<Timeline, SyncobjError> {
        let handle = self
            .fd_to_syncobj(fd, false)
            .map_err(|source| drm_error("import opaque timeline syncobj", source))?;
        Ok(Timeline {
            device: self.clone(),
            handle: Some(handle),
            last_attached_point: 0,
            last_signalled_point: 0,
            last_exported_point: 0,
        })
    }
}

impl AsFd for SyncobjDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for SyncobjDevice {}
impl ControlDevice for SyncobjDevice {}

/// One timeline exported to a Wayland compositor for acquire/release points.
///
/// A presenter imports each Android acquire fence at an odd point and asks the
/// compositor to signal the following even point after it has finished using
/// the committed buffer. One timeline is used per surface so releases remain
/// ordered exactly like that surface's commits.
#[derive(Debug)]
pub struct WaylandTimeline {
    timeline: Timeline,
}

impl WaylandTimeline {
    /// Create an initially empty timeline on the selected render node.
    ///
    /// # Errors
    ///
    /// Propagates DRM syncobj creation failures.
    pub fn create(device: &SyncobjDevice) -> Result<Self, SyncobjError> {
        Ok(Self {
            timeline: device.create_timeline()?,
        })
    }

    /// Export an opaque descriptor for `wp_linux_drm_syncobj_timeline_v1`.
    ///
    /// # Errors
    ///
    /// Propagates the DRM handle-export failure.
    pub fn export_descriptor(&self) -> Result<OwnedFd, SyncobjError> {
        self.timeline.export_opaque()
    }

    /// Import a producer sync-file at the Wayland acquire point.
    ///
    /// # Errors
    ///
    /// Rejects zero, reused, or regressing points and propagates DRM failures.
    pub fn import_acquire_fence(
        &mut self,
        point: u64,
        sync_file: BorrowedFd<'_>,
    ) -> Result<(), SyncobjError> {
        self.timeline.attach_sync_file(point, sync_file)
    }

    /// Return the greatest signalled point without waiting.
    ///
    /// # Errors
    ///
    /// Propagates the DRM timeline query failure.
    pub fn signalled_point(&self) -> Result<u64, SyncobjError> {
        let mut point = [0_u64];
        self.timeline
            .device
            .syncobj_timeline_query(&[self.timeline.handle()], &mut point, false)
            .map_err(|source| drm_error("query Wayland release timeline", source))?;
        Ok(point[0])
    }
}

/// Opaque timeline descriptors attached to one protocol `BindTimelines`.
#[derive(Debug)]
pub struct TimelineDescriptors {
    /// Android-producer to Denial-consumer acquire timeline.
    pub acquire: OwnedFd,
    /// Denial-consumer to Android-producer release timeline.
    pub release: OwnedFd,
}

/// Android-owned acquire and release timelines for one task window.
#[derive(Debug)]
pub struct AndroidTaskTimelines {
    acquire: Timeline,
    release: Timeline,
}

impl AndroidTaskTimelines {
    /// Create two initially empty timelines on the supplied render node.
    ///
    /// # Errors
    ///
    /// Propagates syncobj creation failures. A partially created pair is
    /// destroyed before returning.
    pub fn create(device: &SyncobjDevice) -> Result<Self, SyncobjError> {
        Ok(Self {
            acquire: device.create_timeline()?,
            release: device.create_timeline()?,
        })
    }

    /// Export both timelines as opaque descriptors for one-time task binding.
    ///
    /// # Errors
    ///
    /// Propagates either opaque-handle export failure.
    pub fn export_descriptors(&self) -> Result<TimelineDescriptors, SyncobjError> {
        Ok(TimelineDescriptors {
            acquire: self.acquire.export_opaque()?,
            release: self.release.export_opaque()?,
        })
    }

    /// Import `SurfaceFlinger`'s acquire `sync_file` into a named point before
    /// sending the corresponding descriptor-free `Present`.
    ///
    /// # Errors
    ///
    /// Rejects zero, reused, or reordered points and propagates DRM failures.
    pub fn import_acquire_fence(
        &mut self,
        point: u64,
        sync_file: BorrowedFd<'_>,
    ) -> Result<(), SyncobjError> {
        self.acquire.attach_sync_file(point, sync_file)
    }

    /// Signal an acquire point immediately when `SurfaceFlinger` reports that
    /// rendering completed synchronously and therefore produced no sync-file.
    ///
    /// # Errors
    ///
    /// Rejects zero or regressing points and propagates the DRM signal error.
    pub fn signal_acquire(&mut self, point: u64) -> Result<(), SyncobjError> {
        self.acquire.signal(point)
    }

    /// Export a materialized `SurfaceFlinger` acquire point as a HWC present
    /// `sync_file` without waiting for the rendering operation to complete.
    ///
    /// The direct-render path gives Denial the same acquire point through the
    /// shared timeline. Composer3 also requires a non-null present fence, so
    /// this exports another reference to that exact operation rather than
    /// inventing an unrelated or already-signalled fence.
    ///
    /// # Errors
    ///
    /// Rejects zero, reused, or reordered points and propagates DRM failures.
    pub fn export_acquire_fence(&mut self, point: u64) -> Result<OwnedFd, SyncobjError> {
        self.acquire.export_materialized(point)
    }

    /// Wait until Denial has attached a completion fence to the release point,
    /// then export that point as the Android present `sync_file`.
    ///
    /// This waits for point *availability*, not for the completion fence to
    /// signal. Denial may instead CPU-signal a discarded/hidden frame point.
    /// `absolute_timeout_ns` uses the DRM ABI's absolute monotonic clock.
    ///
    /// # Errors
    ///
    /// Rejects zero, reused, or reordered points and propagates wait, transfer,
    /// and export failures.
    pub fn export_release_fence(
        &mut self,
        point: u64,
        absolute_timeout_ns: i64,
    ) -> Result<OwnedFd, SyncobjError> {
        self.release
            .wait_available_and_export(point, absolute_timeout_ns)
    }
}

/// Denial-imported acquire and release timelines for one task window.
#[derive(Debug)]
pub struct DenialTaskTimelines {
    acquire: Timeline,
    release: Timeline,
}

impl DenialTaskTimelines {
    /// Import the two opaque descriptors from `BindTimelines`.
    ///
    /// # Errors
    ///
    /// Propagates either opaque-handle import failure. A partially imported
    /// pair is destroyed before returning.
    pub fn import(
        device: &SyncobjDevice,
        acquire: BorrowedFd<'_>,
        release: BorrowedFd<'_>,
    ) -> Result<Self, SyncobjError> {
        Ok(Self {
            acquire: device.import_timeline(acquire)?,
            release: device.import_timeline(release)?,
        })
    }

    /// Export a materialized Android acquire point as a renderer/KMS
    /// `sync_file` without waiting for that fence to signal.
    ///
    /// # Errors
    ///
    /// Rejects zero, reused, or reordered points. The kernel rejects a point
    /// which Android failed to materialize before sending `Present`.
    pub fn export_acquire_fence(&mut self, point: u64) -> Result<OwnedFd, SyncobjError> {
        self.acquire.export_materialized(point)
    }

    /// Attach Denial's renderer/KMS completion `sync_file` to a release point.
    /// Android can then export the still-unsignalled point as its present fence.
    ///
    /// # Errors
    ///
    /// Rejects zero, reused, or reordered points and propagates DRM failures.
    pub fn attach_release_fence(
        &mut self,
        point: u64,
        sync_file: BorrowedFd<'_>,
    ) -> Result<(), SyncobjError> {
        self.release.attach_sync_file(point, sync_file)
    }

    /// Materialize and immediately signal the release point for a frame Denial
    /// intentionally discards without submitting renderer or KMS work.
    ///
    /// # Errors
    ///
    /// Rejects zero, reused, or reordered points and propagates DRM failures.
    pub fn signal_release(&mut self, point: u64) -> Result<(), SyncobjError> {
        self.release.signal(point)
    }
}

#[derive(Debug)]
struct Timeline {
    device: SyncobjDevice,
    handle: Option<syncobj::Handle>,
    last_attached_point: u64,
    last_signalled_point: u64,
    last_exported_point: u64,
}

impl Timeline {
    fn handle(&self) -> syncobj::Handle {
        self.handle.expect("live timeline always owns its handle")
    }

    fn export_opaque(&self) -> Result<OwnedFd, SyncobjError> {
        self.device
            .syncobj_to_fd(self.handle(), false)
            .map_err(|source| drm_error("export opaque timeline syncobj", source))
    }

    fn attach_sync_file(
        &mut self,
        point: u64,
        sync_file: BorrowedFd<'_>,
    ) -> Result<(), SyncobjError> {
        let previous = self.last_attached_point.max(self.last_signalled_point);
        require_newer(previous, point)?;
        let temporary = BinarySyncobj::create(&self.device)?;
        import_sync_file_into(&self.device, temporary.handle(), sync_file)?;
        self.device
            .syncobj_timeline_transfer(temporary.handle(), self.handle(), 0, point)
            .map_err(|source| drm_error("transfer sync_file into timeline point", source))?;
        self.last_attached_point = point;
        Ok(())
    }

    fn signal(&mut self, point: u64) -> Result<(), SyncobjError> {
        let previous = self.last_attached_point.max(self.last_signalled_point);
        require_newer(previous, point)?;
        self.device
            .syncobj_timeline_signal(&[self.handle()], &[point])
            .map_err(|source| drm_error("signal timeline point", source))?;
        self.last_signalled_point = point;
        Ok(())
    }

    fn export_materialized(&mut self, point: u64) -> Result<OwnedFd, SyncobjError> {
        require_newer(self.last_exported_point, point)?;
        let temporary = BinarySyncobj::create(&self.device)?;
        self.device
            .syncobj_timeline_transfer(self.handle(), temporary.handle(), point, 0)
            .map_err(|source| drm_error("transfer timeline point into sync_file", source))?;
        let fd = temporary.export_sync_file()?;
        self.last_exported_point = point;
        Ok(fd)
    }

    fn wait_available_and_export(
        &mut self,
        point: u64,
        absolute_timeout_ns: i64,
    ) -> Result<OwnedFd, SyncobjError> {
        require_newer(self.last_exported_point, point)?;
        self.device
            .syncobj_timeline_wait(
                &[self.handle()],
                &[point],
                absolute_timeout_ns,
                true,
                true,
                true,
            )
            .map_err(|source| drm_error("wait for timeline point availability", source))?;
        self.export_materialized(point)
    }
}

impl Drop for Timeline {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.device.destroy_syncobj(handle);
        }
    }
}

#[derive(Debug)]
struct BinarySyncobj {
    device: SyncobjDevice,
    handle: Option<syncobj::Handle>,
}

impl BinarySyncobj {
    fn create(device: &SyncobjDevice) -> Result<Self, SyncobjError> {
        let handle = device
            .create_syncobj(false)
            .map_err(|source| drm_error("create temporary binary syncobj", source))?;
        Ok(Self {
            device: device.clone(),
            handle: Some(handle),
        })
    }

    fn handle(&self) -> syncobj::Handle {
        self.handle
            .expect("live binary syncobj always owns its handle")
    }

    fn export_sync_file(&self) -> Result<OwnedFd, SyncobjError> {
        self.device
            .syncobj_to_fd(self.handle(), true)
            .map_err(|source| drm_error("export timeline point as sync_file", source))
    }
}

impl Drop for BinarySyncobj {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.device.destroy_syncobj(handle);
        }
    }
}

#[repr(C)]
struct DrmSyncobjHandle {
    handle: u32,
    flags: u32,
    fd: i32,
    pad: u32,
}

fn import_sync_file_into(
    device: &SyncobjDevice,
    handle: syncobj::Handle,
    sync_file: BorrowedFd<'_>,
) -> Result<(), SyncobjError> {
    const REQUEST: IoctlRequest = ((IOC_READ_WRITE << IOC_DIR_SHIFT)
        | ((size_of::<DrmSyncobjHandle>() as u64) << IOC_SIZE_SHIFT)
        | (DRM_IOCTL_BASE << IOC_TYPE_SHIFT)
        | (DRM_SYNCOBJ_FD_TO_HANDLE_NR << IOC_NR_SHIFT))
        as IoctlRequest;
    let mut argument = DrmSyncobjHandle {
        handle: u32::from(handle),
        flags: DRM_SYNCOBJ_IMPORT_SYNC_FILE,
        fd: sync_file.as_raw_fd(),
        pad: 0,
    };
    // SAFETY: `REQUEST` is the Linux DRM read/write ioctl for the exact
    // `#[repr(C)] DrmSyncobjHandle` payload. Both descriptors are borrowed and
    // remain live for the call; the kernel may update only this local payload.
    let result = unsafe {
        libc::ioctl(
            device.as_fd().as_raw_fd(),
            REQUEST,
            std::ptr::from_mut(&mut argument),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(drm_error(
            "import sync_file into binary syncobj",
            io::Error::last_os_error(),
        ))
    }
}

fn require_newer(previous: u64, attempted: u64) -> Result<(), SyncobjError> {
    if attempted == 0 {
        Err(SyncobjError::ZeroPoint)
    } else if attempted <= previous {
        Err(SyncobjError::PointRegression {
            previous,
            attempted,
        })
    } else {
        Ok(())
    }
}

fn drm_error(operation: &'static str, source: io::Error) -> SyncobjError {
    SyncobjError::Drm { operation, source }
}

fn deadline_from_parts(
    seconds: libc::time_t,
    nanoseconds: libc::c_long,
    timeout: Duration,
) -> Result<i64, SyncobjError> {
    let seconds = u128::try_from(seconds).map_err(|_| SyncobjError::DeadlineOverflow)?;
    let nanoseconds = u128::try_from(nanoseconds).map_err(|_| SyncobjError::DeadlineOverflow)?;
    if nanoseconds >= 1_000_000_000 {
        return Err(SyncobjError::DeadlineOverflow);
    }
    let now = seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanoseconds))
        .ok_or(SyncobjError::DeadlineOverflow)?;
    let deadline = now
        .checked_add(timeout.as_nanos())
        .ok_or(SyncobjError::DeadlineOverflow)?;
    i64::try_from(deadline).map_err(|_| SyncobjError::DeadlineOverflow)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{SyncobjError, deadline_from_parts, require_newer};

    #[test]
    fn point_zero_is_reserved() {
        assert!(matches!(require_newer(0, 0), Err(SyncobjError::ZeroPoint)));
    }

    #[test]
    fn points_must_advance_strictly() {
        assert!(require_newer(7, 8).is_ok());
        assert!(matches!(
            require_newer(7, 7),
            Err(SyncobjError::PointRegression {
                previous: 7,
                attempted: 7
            })
        ));
        assert!(matches!(
            require_newer(7, 6),
            Err(SyncobjError::PointRegression {
                previous: 7,
                attempted: 6
            })
        ));
    }

    #[test]
    fn ioctl_payload_matches_linux_uapi() {
        assert_eq!(size_of::<super::DrmSyncobjHandle>(), 16);
        assert_eq!(super::DRM_IOCTL_BASE, 0x64);
        assert_eq!(super::DRM_SYNCOBJ_FD_TO_HANDLE_NR, 0xc2);
    }

    #[test]
    fn relative_timeout_becomes_absolute_monotonic_deadline() {
        assert_eq!(
            deadline_from_parts(7, 800_000_000, Duration::from_millis(250)).unwrap(),
            8_050_000_000
        );
        assert!(matches!(
            deadline_from_parts(i64::MAX, 0, Duration::from_nanos(1)),
            Err(SyncobjError::DeadlineOverflow)
        ));
    }
}
