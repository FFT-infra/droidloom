# Droidloom CPU placement: requested reference

The following is the assistant's preceding message, saved at the user's request.
It records the starting findings; implementation evidence and corrections belong
in `docs/architecture.md` and the device workspace.

---

Yes. I found the same placement issue **and a missing Android scheduling integration**.

On the Moto, all 427 sampled tasks allowed CPUs **0–7**. SurfaceFlinger, RenderEngine and Mesa submission workers were last observed on little cores, despite realtime priority.

More significantly, Android’s CPU/cpuset paths are empty directories rather than working controllers. Its `HighPerformance` and `ProcessCapacityHigh` profiles fail. Android already provides the [task-profile abstraction](https://source.android.com/docs/core/perf/cgroups); Droidloom needs to connect it to the host’s controllers.

The allocation should follow these boundaries:

| Work | Placement |
|---|---|
| Wayland presenter, Composer/vsync, SurfaceFlinger and rendering dependencies | Big |
| Catalog, logging, clipboard/notification helpers, known Mesa background queues | Little |
| Android applications and other services | Android’s foreground/background policy |

Composer also declares `ServiceCapacityLow`, targeting `system-background`. That needs correcting when we establish these groups.

I’d reuse Denial’s approach for the host presenter, then repair Android’s profile backend. The kernel already exposes the required controllers.

Saved the findings in [Droidloom’s architecture guide](/mnt/development/droidloom/docs/architecture.md:246). This was inspection only; runtime allocation remains unchanged.
