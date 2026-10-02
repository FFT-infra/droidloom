# Cell camera bridge

The session's `droidloom-camera` service supplies real PipeWire/libcamera frames
through a private Unix stream endpoint. A system-UID Android producer registers
front/back cameras with AOSP `virtual_camera` on default device ID 0. CameraService
keeps application permissions and privacy policy; the producer opens a host stream
only when an application configures an input surface.

The initial input contract is 640×480 packed RGBA at up to 30 fps. The AOSP HAL
handles preview/YUV/JPEG outputs. This is not an assertion of autofocus, flash,
original-vendor image processing or arbitrary resolutions.

## Endpoint and lifecycle

- Host: `%t/droidloom/camera.sock`; Android: `/dev/socket/droidloom/camera`.
- `droidloom-camera.socket` owns the listener. Its parent is mode 0700 and the
  socket is 0666 for the cell bind mount. The host also checks `SO_PEERCRED`,
  accepting only the session owner or Android system UID 1000; ordinary Android
  application UIDs are rejected.
- The service adopts the inherited listener without unlinking it. Restarting the
  camera **service** preserves the inode pinned by the Android mount. Restarting
  the socket unit requires restarting the cell; both units follow the complete
  Droidloom lifecycle.
- At most four connections and one physical stream are admitted. A new stream
  waits at most three seconds for an old pipeline to stop; cancellation is checked
  during the wait and before starting capture. An independently active stream is
  never displaced.
- Socket I/O and missing-frame deadlines are five seconds. The appsink queue holds
  at most two frames. Disconnect, permission-driven stream closure or a failed
  source stops capture; errors are not replaced with black or repeated old frames.

The UID rule describes the current development cell's host-visible Android IDs;
it does not grant a generic arbitrary-UID or network camera API.

## Wire format

All integers are little-endian. Every request is exactly 16 bytes:

| Offset | Field |
|---|---|
| 0 | `DCAM` (4 bytes) |
| 4 | version `u16`, currently 1 |
| 6 | operation `u16`: 1 catalogue, 2 open |
| 8 | camera `u32`: 0 back, 1 front; catalogue requires 0 |
| 12 | reserved `u32`, must be zero |

Every response begins with a 24-byte header:

| Offset | Field |
|---|---|
| 0 | `DCAR` (4 bytes) |
| 4 | version `u16`, currently 1 |
| 6 | kind `u16`: 1 catalogue, 2 opened, 3 frame, 4 error |
| 8 | payload length `u32` |
| 12 | status `u32`: 0 success, 1 protocol, 2 unavailable, 3 busy, 4 capture failure, 5 timeout |
| 16 | timestamp `u64`; zero except for frames |

Catalogue payload is a four-byte availability mask (bit 0 back, bit 1 front).
Opened and error packets have no payload. Each frame has exactly 1,228,800 bytes,
without row padding. Frame timestamps strictly increase and describe monotonic
host delivery time, not a calibrated sensor clock. Clients reject unknown versions,
invalid kinds/statuses, oversized frames and non-increasing timestamps.

## Rendering and host requirements

On a GPU without `GL_EXT_YUV_target`, the patched AOSP HAL renders into an RGBA
framebuffer, reads back pixels and converts with libyuv `ABGRToJ420` (full-range
BT.601). It writes through the allocator's actual Y/CB/CR strides and chroma step,
including planar, NV12 and NV21 layouts. CPU-written outputs do not request the
HAL's GPU render-target usage. Consumer-specified flags are preserved. Registration
probes the chosen output path rather than bypassing capability checks.

The host needs GStreamer core/base libraries, `videorate`, the PipeWire GStreamer
plugin and PipeWire's libcamera support. `droidloom-camera --check` verifies colour
source discovery without opening a sensor. The sheng libcamera simple-pipeline
stop fix and its separate installation boundary are documented in
[`packaging/host/libcamera`](../../packaging/host/libcamera/README.md).

## Partition placement

The producer dequeues gralloc buffers and the HAL links the camera client
library, so both must run from platform partitions: `droidloom-virtual-camera`
is staged into the system image under `/system/bin`, and the producer is a
vendor module installed at `/vendor/bin`. Copies under `/droidloom/...` load
in the default linker namespace and fail at exec ("libandroidicu.so not
found") or on first dequeue ("gralloc-mapper is missing"). The HAL does not
use lazy AIDL registration: init starts it directly, and a lazy registrar
would shut it down before the first client arrives.

Rust tests cover framing, padded rows, credentials, cancellation, handoff and
listener lifetime. `droidloom-camera-protocol-tests`, `virtual_camera_cpu_yuv_tests`
and the non-capturing `droidloom-camera-layout-probe` cover the Android-side
contracts. These checks do not replace real Camera2 preview/JPEG, front/back
switching, permission revocation and repeated shutdown/reopen verification.
