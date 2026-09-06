# Droidloom Android-to-host protocol

Droidloom uses a private, versioned protocol between Android Composer and the
host presenter. It is not a Wayland extension and does not expose a compositor
API. One protocol
task object maps one Android top-level task and its dedicated logical display
to one ordinary Wayland toplevel. A combined Android desktop or phone-screen
object does not exist.

The protocol deliberately reuses Linux graphics primitives instead of
replacing them:

- DMA-BUF file descriptors and explicit DRM format/modifier metadata;
- DRM syncobj timelines for asynchronous acquire and release points;
- Unix `SOCK_SEQPACKET` record boundaries;
- `SCM_RIGHTS` for one-time descriptor transfer;
- peer credentials and a mode-0700 runtime directory for authentication.

DMA-BUF planes are registered once and then referenced by a bounded buffer ID.
Each task shares an acquire and release syncobj timeline once. A steady-state
present carries only the task, buffer, frame, configure serial, damage, and two
timeline-point numbers; it carries no file descriptors and does not wait for a
host protocol response. The host attaches the completed target to Wayland with
explicit synchronization and releases it to Android only after the compositor
signals completion.

The normative version-1 record layout, messages, state transitions, and limits
are documented in [`droidloom-host-v1.md`](droidloom-host-v1.md). The safe
Rust codec in
[`graphics/droidloom-denial-protocol`](../graphics/droidloom-denial-protocol)
is the executable definition. The audited Rust adapter in
[`graphics/droidloom-denial-ipc`](../graphics/droidloom-denial-ipc) implements
sequenced records, peer credentials, `SCM_RIGHTS`, close-on-exec receipt, and
exact FD-role binding. Its filesystem listener refuses to replace existing
paths, and the endpoint checks configured peer credentials reported by
`SO_PEERCRED` before parsing `ClientHello`. UID/GID remain exact; the development
configuration allows a PID wildcard until supervisor-driven PID registration is
implemented. The
[`droidloom-syncobj`](../graphics/droidloom-syncobj) wrapper performs the
opaque timeline and `sync_file` transfers, while the concrete Android sink and
host endpoint join them to task presentation state.

The public presentation boundary is the compositor's normal `xdg-shell`,
`linux-dmabuf`, and `linux-drm-syncobj` implementation. The private protocol
exists only because Android Composer is not itself a Wayland client.
