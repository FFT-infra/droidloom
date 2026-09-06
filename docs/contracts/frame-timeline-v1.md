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

- A ready commit is eligible only at its named latch.
- A commit missing that latch is discarded; the previous surface contents
  remain current.
- A late commit is never relabelled onto a later grant.
- Multiple late commits are never burst to catch up.
- Expired, unissued, wrong-epoch, inactive, and regressing targets discard only
  their next buffer update; they are scheduling misses, not client-fatal
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
