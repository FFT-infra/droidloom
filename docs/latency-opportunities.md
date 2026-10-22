# Latency opportunities

Where Droidloom can lose time between a touch and the frame that shows it, and
what could be done about each. Items marked *confirmed* are visible in the code;
the rest are hypotheses to measure first. Nothing here has device numbers yet.

## Two clocked compositors

On native Android, SurfaceFlinger is the last stage before the display. Here it
is a middle stage: SF wakes on Composer's synthetic vsync, sends the scene to
the presenter, and Denial then waits for its own vblank. Each clocked stage can
cost up to a frame, so Droidloom likely pays one frame (8.3 ms at 120 Hz) more
than native Android.

- SF timing is untuned: the product files set no `debug.sf.*` durations or
  offsets, so SF uses defaults that know nothing about Denial's latch.
- The synthetic vsync free-runs. Its phase is seeded only when SF enables HWC
  vsync ([service.rs](../graphics/droidloom-composer-aidl/src/service.rs),
  end of `observe_presentation`), and SF disables vsync once its model is
  learned, so drift against the real panel accumulates. The vsync audit's
  `phase_error_max` shows how much.

Options, smallest first:

1. Slew the running clock toward Denial's presentation timestamps instead of
   jumping it.
2. Set SF's duration so it wakes just before Denial's latch deadline.
3. For direct-layer tasks, send the scene when the app's transaction arrives,
   without waiting for SF vsync. SF renders nothing on that path; Denial paces.
   This removes the extra stage instead of shrinking it.
4. Use the [frame timeline](contracts/frame-timeline-v1.md) both ways: Android's
   expected present time to Denial, Denial's real deadlines back as vsync.

## Input

1. **Delivery-time stamps** (confirmed). Denial puts libinput's hardware time in
   `wl_touch`; the presenter discards it and stamps each event with
   `clock_gettime` on receipt (`send_input` in
   [main.rs](../graphics/droidloom-wayland/src/main.rs)). Android resamples at
   vsync and computes fling velocity from event times, so delivery jitter
   becomes wobbly scrolling and wrong fling speed. Carry the hardware time;
   `zwp_input_timestamps_v1` in Denial would give nanosecond precision.
2. **Clipboard gate holds all input** (confirmed). Every
   `wl_data_device.selection`, which is re-sent on keyboard focus, blocks touches
   until the clipboard is read and imported into Android, up to 10 s
   (`selection` in [clipboard/mod.rs](../graphics/droidloom-wayland/src/clipboard/mod.rs)).
   A screenshot PNG on the clipboard delays the first tap after switching to an
   Android app. Gate paste instead of input, or send the description and fetch
   data when Android reads it.
3. **`wl_touch.frame` is ignored.** Each point is sent and injected alone, so two
   moving fingers become two MotionEvents per hardware frame, the first holding
   the other finger's old position. Likely pinch jitter. Buffer until `frame`
   and inject one MOVE.
4. **Receiver thread priority.** `DroidloomInput` in system_server runs at
   default priority ([input_ffi.rs](../android/framework/droidloom-window-policy/src/input_ffi.rs)),
   while InputReader and InputDispatcher run at urgent-display priority. Match
   them and use the graphics CPU placement.
5. **One presenter loop.** Input shares the poll loop with scene imports, fences
   and clipboard, so it can wait behind graphics work. Probably small; one
   trace would tell.

## Frames

1. **Late buffer release** (confirmed, Denial side). Denial signals a client's
   release point on the CPU after its own GPU fence signals
   (`publish_sampled_buffer_release` in deniald's renderer handler). Native SF
   returns a release fence at latch. Buffers therefore come back about a frame
   plus GPU time later, and with three app buffers and two compositors a busy
   app can block in `dequeueBuffer`. Transfer Denial's render fence into the
   release point at submit; the presenter already exports a release fence as
   soon as it exists.
2. **Per-app release lanes.** Release callbacks are ordered per app process, so
   one held buffer stalls every later callback for that app (the cause of the
   2026-10-19 lag). Order per surface, or deliver transaction-complete callbacks
   without waiting on release.
3. **Driver CPU cost.** Telegram's drops were mostly "slow issue draw commands".
   The image sets neither `debug.hwui.renderer` nor `ro.hwui.use_vulkan`, so
   HWUI likely uses GL on freedreno rather than Vulkan on Turnip. A/B with
   `debug.hwui.renderer=skiavk`.
4. **Buffer layout.** minigbm builds for `msm`. If app buffers end up linear
   rather than UBWC, both app rendering and Denial's sampling pay full
   bandwidth at 120 Hz. Check the modifiers the presenter imports.
5. **GPU clock for Android work.** Denial's GPU floor follows touches and its
   own animations, not Android animations after lift (flings, transitions),
   which stay at the `simple_ondemand` floor. Forward power HAL boosts and hint
   sessions to the host.
6. **Full-screen Android apps still go through Denial's GPU.** An opaque
   full-screen buffer could scan out on a KMS plane and skip Denial's raster
   and GPU pass. Larger, Denial-side.
7. **App compilation.** Background dexopt needs idle and charging; if the cell
   never reports them, apps stay on JIT. Check `dumpsys package dexopt`.
8. **Telegram at 60 fps on a 120 Hz panel** is still open. Check SF's selected
   refresh rate.

## Measure first

Current traces start at SF. One end-to-end number is missing: libinput hardware
time, presenter receipt, injected event time, then the presentation timestamp
of the first frame that reflects the touch. Without it every item above is a
guess; with it, the double clock and the timestamp issue should both show in a
single drag.

Expected order of payoff: clock alignment or bypass, touch timestamps, early
release fence.
