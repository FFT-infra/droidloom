# Droidloom Android-to-host protocol version 1

Status: accepted transport contract  
Byte order: little endian  
Transport: connected Unix `SOCK_SEQPACKET` socket

## Scope

One authenticated connection represents one Android cell belonging to the
logged-in Linux user. Each Android top-level task is a separate protocol object
and one ordinary Wayland toplevel. There is no root object representing an
Android desktop, emulator display, or phone screen.

Version 1 supports the SurfaceFlinger client-target correctness path, cached
DMA-BUF import, explicit DRM syncobj timeline synchronization, presentation
timing, native task state, touch/key/navigation input, and task teardown. It
does not support protected content, HDR presentation, delegated Android layer
trees, capture, or a software-rendering fallback.

This protocol does not redefine Linux allocation or synchronization. DMA-BUF,
DRM FourCC/modifiers, DRM syncobj timelines, `sync_file`, Unix credentials, and
`SCM_RIGHTS` remain the underlying kernel ABIs.

## Connection and authentication

The host presenter creates the socket below the user's mode-0700 runtime
directory. The supervisor bind-mounts only that socket into the Android cell.
The presenter validates the peer credentials and associates them with the
expected cell before parsing `ClientHello`.

Task and package IDs received from Android are claims. The host reconciles them
with the trusted session service before exposing launcher, close, focus, or
input authority. Authentication never depends on a package string in a packet.

A connection begins with `ClientHello` and `ServerHello`. Version 1 requires
all of these capabilities:

- independent native task windows;
- cached DMA-BUF imports;
- shared DRM syncobj timelines;
- presentation timing;
- native input routing;
- one SurfaceFlinger client target per task display.

Unknown capability bits are ignored during negotiation. Missing required bits
are fatal. This private connection complements the presenter's public Wayland
connection; it never represents a combined Android display.

## Packet header

Every sequenced record contains exactly one header and payload. Partial records
are impossible at the protocol layer; truncation (`MSG_TRUNC` or
`MSG_CTRUNC`) is fatal.

| Offset | Type | Field | Rule |
| ---: | --- | --- | --- |
| 0 | `[u8; 4]` | magic | ASCII `DLOM` |
| 4 | `u16` | major | `1` |
| 6 | `u16` | minor | `0` |
| 8 | `u16` | opcode | direction-specific value |
| 10 | `u16` | descriptor count | must equal ancillary descriptors |
| 12 | `u32` | flags | zero in version 1 |
| 16 | `u64` | object ID | zero for connection messages, otherwise non-zero |
| 24 | `u32` | payload bytes | exact remaining record length |
| 28 | `u32` | reserved | zero |

The complete record is at most 65,536 bytes. Integers are little endian.
Strings are a `u16` byte length followed by valid UTF-8 without a trailing NUL.
Unknown opcodes, unknown enum values, non-zero reserved bits, trailing bytes,
and descriptor-count mismatches are fatal protocol errors.

Descriptors attached with `SCM_RIGHTS` have message-defined roles and order.
The receiver closes the connection and every received descriptor when a record
is malformed. Descriptors are never inferred from payload integers.

## Android-to-host messages

| Opcode | Message | Object | Attached descriptors |
| ---: | --- | --- | --- |
| `0x0001` | `ClientHello` | connection | none |
| `0x0002` | `CreateTask` | new task object | none |
| `0x0003` | `DestroyTask` | task | none |
| `0x0004` | `BindTimelines` | task | acquire timeline, release timeline |
| `0x0005` | `AckConfigure` | task | none |
| `0x0006` | `RegisterBuffer` | task | one DMA-BUF FD per dense plane |
| `0x0007` | `UnregisterBuffer` | task | none |
| `0x0008` | `Present` | task | none |
| `0x0009` | `SetContentState` | task | none |
| `0x000a` | `SetFrameRate` | task | none |
| `0x000b` | `Pong` | connection | none |

`CreateTask` carries a non-zero Android task ID, a non-zero dedicated logical
display ID, and a package identity of at most 255 bytes. Object, task, and
display identities cannot be reused while live.

`BindTimelines` transfers exactly two DRM syncobj opaque descriptors. The first
exports the acquire timeline; the second exports the release timeline. Both
sides retain imported handles for the lifetime of the task. Rebinding is a
protocol error.

`RegisterBuffer` carries an allocation ID, dimensions, explicit DRM FourCC and
modifier, and one through four dense plane records containing index, offset,
and stride. Its ancillary descriptors contain those planes in index order. A
buffer ID cannot be registered with different immutable metadata.

`Present` carries a unique frame ID, registered buffer ID, acknowledged
configure serial, acquire point, release point, and no more than 256 damage
rectangles. It is descriptor-free. Points are non-zero and monotonically
increase on their respective task timelines.

Before sending `Present`, the Android adapter imports SurfaceFlinger's acquire
`sync_file` into the named acquire point and allocates a release point. After
receiving `Present`, the host imports that operation into the matching Wayland
surface timeline. It signals the Android-facing release only after the Wayland
compositor signals its release point. If it discards a headless frame, it
signals release explicitly. Android waits for point *availability*, not fence
completion, and then exports the still-unsignalled point as the present fence
returned to SurfaceFlinger. The handoff uses shared kernel objects and requires
no synchronous protocol reply or per-frame socket descriptor.

## Host-to-Android messages

| Opcode | Message | Object | Attached descriptors |
| ---: | --- | --- | --- |
| `0x8001` | `ServerHello` | connection | none |
| `0x8002` | `FormatFeedback` | task | none |
| `0x8003` | `Configure` | task | none |
| `0x8004` | `Insets` | task | none |
| `0x8005` | `Visibility` | task | none |
| `0x8006` | `Close` | task | none |
| `0x8007` | `BufferReleased` | task | none |
| `0x8008` | `Presented` | task | none |
| `0x8009` | `Input` | task | none |
| `0x800a` | `Error` | connection or task | none |
| `0x800b` | `Ping` | connection | none |

`FormatFeedback` atomically replaces the accepted explicit FourCC/modifier set
for its generation. At most 256 pairs are sent. `DRM_FORMAT_MOD_INVALID` is not
accepted because implicit modifier negotiation is outside the version-1 path.

`Configure` supplies a serial, non-zero buffer size no larger than 16,384 by
16,384 pixels, rational scale, output transform, and refresh rate. Android must
acknowledge the latest serial before presenting the initial or resized buffer.

After waiting for the acquire point, the Wayland compositor may sample,
promote, or directly scan out the buffer. When all GPU/KMS use finishes, it
signals the Wayland release point; the presenter then signals the named
Android-facing release point and sends `BufferReleased`. Android uses that
event to retire its internal in-flight frame.

`Presented` is independent from release because scanout timing and safe buffer
reuse are different events. It carries a monotonic timestamp, refresh period,
sequence, and whether the frame was composed or directly scanned out.

`Input` carries a monotonic host sequence and timestamp plus a typed touch, key,
or navigation event. Android never receives the host's physical input
device descriptors.

## Required state ordering

For one task, the minimum successful sequence is:

```text
CreateTask
  -> BindTimelines(acquire timeline FD, release timeline FD)
  <- FormatFeedback(generation, formats/modifiers)
  <- Configure(serial, size, scale, transform, refresh)
  -> AckConfigure(serial)
  -> RegisterBuffer(buffer metadata, DMA-BUF plane FDs)  [once per allocation]
  -> Present(buffer ID, frame ID, acquire point, release point, damage)
  <- Presented(frame ID, timing)                         [when displayed]
  <- BufferReleased(frame ID, buffer ID, release point) [after all use]
```

No buffer may be presented before timeline binding, compatible feedback, and
the latest configure acknowledgement. A buffer cannot be presented again
while an earlier frame using it remains unreleased. A task cannot be destroyed
while a frame is in flight. Disconnect closes all task windows and imported
descriptors through presenter-owned bounded teardown.

## Bounds and failure policy

Version 1 admits at most 4,096 task objects per cell, 256 registered buffers
per task, four planes per allocation, 256 format/modifier pairs per feedback
generation, and 256 damage rectangles per frame. Implementations may negotiate
smaller task, buffer, and damage limits in `ServerHello`.

Malformed packets, stale configuration, descriptor ambiguity, duplicate IDs,
point regression, unsupported formats, protected content, resource exhaustion,
and release-before-use violations fail closed. The host may send one bounded
`Error` record and then disconnect; correctness never falls back to pixel
readback, implicit synchronization, or a nested Android desktop.

## Performance contract

The protocol design makes three testable claims:

1. Each allocation's DMA-BUF descriptors cross the socket once per import
   lifetime, not once per frame.
2. Timeline descriptors cross once per task; a normal `Present` and its release
   notification are descriptor-free.
3. All state needed to latch a client-target frame is contained in one bounded
   `Present` record, with no synchronous host response in the Composer thread.

These properties keep the private Android boundary efficient while the host
uses ordinary Wayland for presentation. Release evidence must measure bridge
CPU time, bytes, messages, syscalls, wakeups, imports, scheduling latency,
copies, and frame misses under the same workload.
