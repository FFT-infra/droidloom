# libcamera 0.7.1 stop-lifecycle fixes

This optional **host** patch fixes the simple pipeline's stop cleanup. Requests
already submitted to the software ISP leave `conversionQueue_`, but can still
wait for IPA metadata. Clearing `frameInfo_` without cancelling them leaves the
framework request queue nonempty. The patch cancels all tracked requests after
stopping the ISP/video device and removes records before completion callbacks.
The framework's empty-queue assertions remain enabled.

The second patch disconnects capture delivery before stopping the ISP. An
isolated IPA's synchronous stop can dispatch capture events after the worker
has exited, leaving invocations carrying freed buffers for its next start.
Quiescing that producer makes the existing blocking worker stop and owner
callback drain sufficient before buffer release; no extra barrier or retry is
added.

This does not add an Android Camera Provider and is not installed by Droidloom.

## Regression

The harness extracts the real cleanup/completion methods from the supplied
`simple.cpp`; only hardware, buffers and the framework queue are simulated.
It covers queued conversion, cancelled ISP output, completed output lacking
metadata, normal completion, callback ordering, exactly-once completion and
late/reentrant metadata.

```sh
rustc --edition=2024 packaging/host/libcamera/stop-regression.rs -o "$BUILD_ROOT/stop-regression"
"$BUILD_ROOT/stop-regression" /path/to/libcamera/src/libcamera/pipeline/simple/simple.cpp \
  "$BUILD_ROOT/new-test-output"
```

The output directory must not exist. Unpatched v0.7.1 fails five of eleven
checks; patched source passes all eleven. The RPM build runs this red check
before applying the patch, then the green check in `%check`.

`worker-regression.rs` extracts both production shutdown methods and links the
built `libcamera-base` for real `Thread`, `Object` and `Signal` behavior. Capture
and IPA events are simulated, with lifetime markers instead of image buffers.
It runs five red/green pairs: the first-patch baseline must replay an expired
frame and deliver two stale owner callbacks; the two-patch candidate must do
neither, while still draining earlier callbacks before release. `%check`
reconstructs that baseline from the original tarball and first patch. The
traces survive RPM cleanup as `rpmbuild/worker-baseline.log` and
`rpmbuild/worker-candidate.log`.

The wider upstream suite is separate: run `meson test -C redhat-linux-build`
from a prepared libcamera build tree on a suitable native host. Its process/IPC
tests require native `clone()` support, and its GStreamer virtual-camera tests
need a DMA-buffer allocator. QEMU user emulation and a device-free container do
not satisfy those contracts; report their failures/skips separately. Neither
the focused harness nor compilation proves physical camera stability.

## Rootless ARM64 RPM build

Requirements: Podman, working ARM64 binfmt/QEMU on non-ARM64 hosts, curl and
sufficient space outside the repository. The pinned Fedora 44 image installs
build dependencies **inside the container only**. No host sudo is used.

Use a new build directory; do not overwrite an existing candidate:

```sh
: "${BUILD_ROOT:?Set an unused absolute build directory outside the repository}"
mkdir -p "$BUILD_ROOT/context" "$BUILD_ROOT/output"
cp packaging/host/libcamera/Containerfile packaging/host/libcamera/*.rs \
  packaging/host/libcamera/*.cc.in packaging/host/libcamera/*.patch "$BUILD_ROOT/context/"
curl --fail --location \
  https://kojipkgs.fedoraproject.org/packages/libcamera/0.7.1/1.fc44/src/libcamera-0.7.1-1.fc44.src.rpm \
  --output "$BUILD_ROOT/context/libcamera-0.7.1-1.fc44.src.rpm"
podman build --arch arm64 --jobs 2 -t localhost/libcamera-stop:0.7.1 "$BUILD_ROOT/context"
podman image inspect localhost/libcamera-stop:0.7.1 --format '{{.Id}}' > "$BUILD_ROOT/builder-image-id.txt"
podman run --rm --arch arm64 --cpus 2 -v "$BUILD_ROOT/output:/work:Z" \
  localhost/libcamera-stop:0.7.1 /work/run
```

The source RPM SHA-256 is pinned in `Containerfile` and `build.rs`:
`2ab6c01e0191dec0599076a7451d0c3978ef4e045802d3424939d748bd756647`.
The build keeps **Version 0.7.1 and Release 1.fc44 unchanged**. It adds only the
stop patches, self-contained regression sources and their Rust compiler dependency,
and the missing `elfutils-devel` build dependency to Fedora's original spec.
The patched SRPM contains everything needed for its `%check`; it does not rely
on a preinstalled private test executable. RPMs, the patched
SRPM and patch checksum are in `output/run/candidate/`; the source digest,
build-environment inventory, red result and complete build/test log remain in
`output/run/`. Dependency versions are recorded, not frozen; this is a pinned
source build, not a claim of byte-identical RPM reproducibility.

## Installation and rollback boundary

Installation is a separate, explicitly coordinated operation. Before replacing
anything, retain the original `libcamera` and `libcamera-ipa` ARM64 RPMs from the
same Fedora build, record their checksums, and coordinate users of the libraries.
Do not overwrite mapped library files manually. Identical NEVRA values do not
identify package contents: verify the installed library hashes against the
selected candidate and record which original hashes restore the previous state.

Physical acceptance requires repeated bounded front/rear open, stream, close
and switch operations, unchanged WirePlumber PID, no queued-request assertion,
and sensors returning to idle. A failed camera test must not be hidden with
service-restart loops. Keep downloaded trees, logs and RPMs out of Git.
