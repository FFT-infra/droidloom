# Framework integration boundary

`droidloom-home` is the cell's idle HOME destination: one SDK-only Java activity
with an opaque static background and hardware acceleration disabled. It has no
launcher entry, permissions, services, timers, or wake locks. Its task stays alive
under Android's ordinary lifecycle and is never registered for native export.
This lets Settings' `FallbackHome` finish instead of polling and animating forever.

The independent `droidloom-home-setup` init oneshot installs the signed APK and
selects it as user 0's default HOME after boot. The APK and script use the existing
`/droidloom/ime` support directory, shared with keyboard provisioning. Updates
replace the APK only when its digest changes and reassert HOME resolution on every
boot. After verifying HOME resolves to DroidloomHome, setup disables stock
`com.android.launcher3` for user 0 and force-stops any residual process.
Its base APK and data are retained; no Settings component is disabled. To undo
that package policy, use Android's `cmd package enable --user 0 com.android.launcher3`
and change provisioning before the next boot.
The existing composer init file imports `/droidloom/ime/droidloom-home.rc`, so
provisioning does not require adding a file to the immutable vendor image.

`droidloom-systemui` replaces the stock SystemUI APK with a platform-signed,
persistent service built from scratch. It registers the pinned `IStatusBar`
interface and implements the minimum keyguard boot callbacks, with no windows,
render loop, notification renderer, or resource bundle. Generated Binder
adapters preserve the platform transaction layout and reject callers outside
root/system. Regenerate them with `python3 tools/droidloom-systemui-stubs` when
advancing the pinned framework interfaces; `--check` verifies the checked-in
adapters. The updater also checks their source hashes against the build inputs.

Unsupported biometric prompts cancel without credential attestation, quick
settings tile requests return a negative result, and text toasts complete their
hidden callback and release the framework token. Screenshot requests complete
without an image, media projection requests cancel, and shell/proto output file
descriptors close. A cell with an Android credential configured is reported as
locked; this initial service has no Android credential-entry UI. Desktop locking
and notification presentation belong to the host.

The same process hosts event-driven clipboard and notification adapters. Private
UID-authenticated sockets connect them through the existing input bridge to the
Rust Wayland presenter. The notification listener sends bounded active snapshots
to `org.freedesktop.Notifications` on the desktop session bus. Android updates
replace desktop IDs; removals close them. Default clicks and ordinary action
buttons invoke version-checked Android PendingIntents. Explicit desktop dismissal
cancels clearable Android notifications; desktop expiry does not.
Ongoing Android notifications also use the desktop's normal popup timeout; their
ongoing flag must not pin a popup indefinitely. Android LOW/MIN channel importance
maps to low desktop urgency; DEFAULT/HIGH maps to normal urgency. Channel
importance alone never requests a non-expiring critical desktop alert. Group summaries
are skipped to avoid duplicating their children. Inline replies and custom Android
notification layouts have no mapping in this initial standard D-Bus adapter.
Retry timers run only while a transport is disconnected.

`DroidloomNotificationProbe` and the presenter's `notification-test-server`
example support headless integration checks on a private D-Bus. The probe's
actions use broadcasts and its channel has no sound or vibration. Neither test
component belongs in production packaging. Visible desktop tests remain subject
to the workspace's explicit-trigger rules.

The APK overlays `/system_ext/priv-app/SystemUI/SystemUI.apk`. Because Android's
package parser cache uses the immutable containing directory's timestamp, the
supervisor invalidates only SystemUI's parser cache before Android starts. This
also covers rollback to the stock APK. The `DroidloomSystemUIProbe` build target
checks callback completion and Binder caller guards under Android shell UID
2000, without creating windows, prompts, notifications, or input events.

Only narrowly reviewed Android framework patches belong here. Package/task,
notification, clipboard, inset, lifecycle, and power-policy integration should
prefer a Droidloom system service over broad framework changes.

`droidloom-task-launcher` is the initial Rust task bridge. The privileged host
supervisor executes it inside the Android cell as root/system. It launches
ordinary freeform tasks on the shared built-in display through Android's pinned
`cmd activity` interface, discovers the exact leaf task, and binds its real task
ID to the host window. This does not require a full AOSP checkout.

The root input bridge also registers a platform `TaskStackListener`. When Android
brings a visible standard application task to the foreground on user 0/display 0,
the observer binds that existing task through `droidloom-task-launcher --bind-task`.
This covers Play Store's Open button, deep links, shares, choosers and activity
PendingIntents without starting another launcher activity. Android retains the
original intent, extras, URI grants, task flags and activity-result relationship.
HOME, recents, organizer containers, background tasks and other users/displays
are excluded. Activities from another package within the same task retain that
task's base owner, including authentication and permission screens.

Task callbacks are coalesced on a worker thread, with bounded retries for failed
registration and no idle task polling. Concurrent explicit launches and observer
registration serialize through the same idempotent task registry. Reopening an
existing task requests host activation through the negotiated task-activation
capability. The Wayland presenter uses `xdg_activation_v1` and a recent input
serial when available; the host compositor decides whether to grant focus.
Update the input bridge, task launcher, Composer and presenter together for
this path. Older hosts can still create task windows but cannot honor the new
activation request.

The observer policy has a local JVM check:

```console
javac -d .work/task-observer-tests android/framework/droidloom-input-bridge/src/com/android/droidloom/input/TaskRegistration.java android/framework/droidloom-input-bridge/tests/com/android/droidloom/input/TaskRegistrationTest.java
java -cp .work/task-observer-tests com.android.droidloom.input.TaskRegistrationTest
```

First-launch permission dialogs remain part of the requesting app's task. The
launcher resolves Android's system permission handler and accepts that exact
activity during task discovery, while still requiring the requested app's base
task identity, Android user, and display. It also handles the dialog appearing
or closing during discovery. Permissions are left for the user to choose through
Android's normal dialog.

`droidloom-input-bridge` converts the compact task-tagged records received from
Denial into ordinary Android input events. Its two private operations terminate
in Droidloom's native SurfaceFlinger and InputFlinger builds: SurfaceFlinger
resolves a rendered task to its existing input-application token, and
InputFlinger constrains normal dispatch to that token. The bridge never changes
task focus or stacking. The input integration leaves boot `framework.jar`,
public AIDL, and all stock Binder transaction numbers unchanged.

The same package contains the one-shot `ApplicationCatalog` framework adapter.
It lets Android's package manager and resource stack resolve enabled launcher
activities, localized labels, and adaptive icons for the unprivileged host
catalog service. Linux never parses APK manifests or resources.

`0001-droidloom-bottom-navigation-insets.patch` is the narrow system-server
exception for host system bars. Android's freeform policy strips system-bar
insets, so the patch supplies visible 33.6 dp top status-bar and 16.8 dp bottom navigation
sources, plus bottom mandatory gestures, after that policy and to activity
WindowMetrics. It is enabled
only by Droidloom's existing read-only task-window product marker. See
[`navigation-insets.md`](../../docs/contracts/navigation-insets.md) for the API,
explicit services build, packaging and validation contract.
