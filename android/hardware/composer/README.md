# Droidloom Composer boundary

The safe Rust state machine lives in graphics/droidloom-composer. It models
the initial HWC3 correctness path: one host-managed Android logical display
and one native Denial toplevel per top-level task, every unprotected layer
corrected to client composition, and one Surface Flinger client target per task
submitted through the existing DMA-BUF/DRM-syncobj transport. It cannot create
a combined Android desktop/emulator window.

aidl-lock.json pins Composer 3 version 5 and allocator version 3 to the exact
frozen API hashes found at the locked Android 17 hardware/interfaces commit.
The Android Binder adapter must use generated Rust Composer V5 bindings. The
allocator remains NDK AIDL because the pinned upstream interface does not
enable a Rust backend; mapper uses the stable-C ABI version 5.

The Soong-only Rust AIDL layer in graphics/droidloom-composer-aidl implements
the full frozen V5 `IComposer` and `IComposerClient` method surface around the
bounded command/result translator. It reports only the minimum physical-display
capabilities that the safe core actually implements; virtual display, readback,
HDR conversion, HDCP, and other incomplete features fail with the frozen
service-specific `UNSUPPORTED` result. Native handle, buffer-slot, and fence
ownership is isolated behind its `NativeBufferAdapter` trait so the bridge
cannot guess gralloc metadata or fabricate a fence.

The safe Rust parser in graphics/droidloom-minigbm validates the exact pinned
`cros_gralloc_handle` word layout, plane/reserved-FD counts, dimensions,
strides, offsets, sizes, FourCC, and explicit modifier. The Android-only
`MinigbmBufferAdapter` owns bounded per-display client-target slots and
per-layer metadata slots, duplicates real plane/acquire descriptors, rejects
protected content, and hands accepted targets to a `PresentationSink`. The
concrete sink registers allocation descriptors once, imports acquire fences
into shared DRM-syncobj points, sends one descriptor-free `Present`, waits for
Denial to materialize (not signal) the release point, and returns its real
`sync_file` to Surface Flinger.

The Binder service/client layer remains intentionally thin:

1. decode and bound AIDL command payloads;
2. translate display/layer/validate/accept/client-target/present operations to
   droidloom-composer;
3. register native handles with the Denial protocol and import/export
   `sync_file` fences through the task's shared DRM syncobj timelines;
4. return explicit failures for every feature marked unsupported in
   service-contract.json.

The repository root declares vendor-only Soong libraries for the tested safe
transport, Denial-native protocol/socket adapter, DRM-syncobj wrapper, minigbm
parser, and Composer cores plus the Composer V5 Binder layer. Its ARM64
Composer executable opens the render node and private Denial socket, performs
the native handshake, registers `IComposer/default`, and pumps configure,
presentation, and release events. It also product-packages upstream minigbm's
Allocator V3 service and Mapper V5 provider with their init/VINTF fragments.
The focused build therefore selects the vendor variants rather than asking
Soong to build unrelated platform variants.

The Composer executable is included in `PRODUCT_PACKAGES` with an init-owned
root/system-only sequenced task-control socket and a Composer V5 VINTF fragment.
It still exposes no combined Android desktop: displays are created only by an
explicit task reservation after the authenticated Denial endpoint is live.
