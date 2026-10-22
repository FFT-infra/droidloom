# Exact frame timeline contract

`denial_frame_timeline_v1` is an optional, product-neutral Wayland extension.
It adds a scheduling constraint to ordinary `wl_surface` commits; it does not
create a window role or an alternate scene, input, resize, animation, or KMS
path.

## Identity and time

A frame grant is:

```text
FrameGrant {
    epoch: u64,
    sequence: u64,
    latch_deadline_ns: u64,
    presentation_target_ns: u64,
    refresh_period_ns: u64,
}
```

Only `(epoch, sequence)` is identity. Times use the compositor's
`wp_presentation` clock and are estimates used for scheduling. Presentation
feedback never changes the identity of an issued grant.

The compositor announces a grant before the producer must start. At a nominal
period `P`, a producer budget `A`, and guard `G`, the intended schedule is:

```text
T - (P + A + G)  producer work begins
T - P            compositor latch deadline
T                physical presentation target
```

## Commit rules

`set_target(epoch, sequence)` is double-buffered state consumed by the next
`wl_surface.commit`.

- A commit is latched for its grant only if it is ready by the named latch.
- A commit missing that latch counts as a miss for its grant. It is not
  discarded: it applies untargeted once ready, is shown at the next
  opportunity, and its buffer is released like any other.
- A late commit is never relabelled onto a later grant.
- Multiple late commits are never burst to catch up.
- Expired, unissued, wrong-epoch, inactive, and regressing targets leave only
  their commit untargeted; they are scheduling misses, not client-fatal
  protocol errors.
- Cross-client object references remain protocol errors.
- Untargeted clients retain normal Wayland behavior.
- `wp_presentation` is the completion record for accepted commits. It is not a
  clock-control input.

## Epochs and loss

An output mode, output identity, or timeline reset increments the epoch. Every
outstanding identity in prior epochs is aborted. A client must drain its local
work and wait for the first grant in the new epoch.

Loss of the exact-timeline object pauses an exact-mode producer. It must not
silently fall back to a private timer. A separately selected portable mode may
use standard frame callbacks and presentation feedback, with weaker semantics.

## Terminal accounting

Every consumed grant ends in exactly one state:

- presented;
- intentionally skipped because no content changed;
- missed deadline;
- discarded by configure/resize;
- aborted by epoch change.

Android GPU completion, Wayland buffer release, and physical host presentation
remain three independent synchronization events.

## Droidloom presenter

`graphics/droidloom-wayland` selects exact mode automatically when the global
is advertised at startup. Without it, ordinary portable commits and existing
presentation feedback continue unchanged. Exact mode verifies that
`wp_presentation` uses CLOCK_MONOTONIC, as required by Droidloom's host timing
records, before subscribing to an output.

Each task subscribes to its presentation output, then its entered output, or
an available output for the first mapping. It retains at most one composed
frame awaiting a grant, or the layer stream's existing single pending scene.
A newer submission can supersede work which has not consumed a grant. Once
consumed, the identity is sent with `set_target` before the corresponding
buffer commit, including changed synchronized children. A late commit keeps
its identity; the compositor records the miss and shows it untargeted. No
presentation timestamp is used to reconstruct an identity or to drive a
private timer.

Completion metadata is bounded to 64 composed-frame callbacks and eight scene
callbacks per task. Exact producers wait for feedback capacity; portable mode
can omit optional observations. Scene and composed callbacks share terminal
cleanup on configure, epoch changes and window destruction. Output changes
discard queued work and subscribe again; losing the global pauses existing
exact producers. None of these timing operations releases a submitted GPU
read or substitutes for the compositor's buffer-release fence.

The shared XMLs are checked byte for byte against Denial's `dev` checkout by
the Shared Wayland protocols CI workflow. To check a local Denial tree:

```sh
DROIDLOOM_DENIAL_SOURCE=/mnt/development/denial-os/denial cargo test --locked -j 1 -p droidloom-contracts --test repository_contracts shared_wayland_protocols_match_denial
```
