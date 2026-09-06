# Desktop integration

## Windows

`droidloomctl start` and `droidloomctl stop` control the complete runtime. Startup
is manual. The catalog exports Android launcher activities, labels and icons as
XDG desktop applications. `droidloomctl applications` lists the catalog; launches
require a running cell.

Desktop mode starts with a 480×800 logical window, constrained to available space,
and remembers app sizes. `droidloomctl start --mode mobile` fills current bounds.
Compositor sizes take precedence; restart preserves the mode. `window-mode`
supports per-app preferences. Android's privileged task-resize API updates task
configuration and crop. See [insets and resizing](contracts/navigation-insets.md).

## Clipboard

The presenter exchanges clipboard changes with an adapter in Android SystemUI.
Text, links, HTML, encoded images and copied files are supported when applications
accept the format. Android content URIs use a scoped provider, not Linux paths.

The host prefers ext-data-control, supports wlr-data-control and falls back to
core Wayland clipboard access. The core path needs keyboard focus and an input
serial for selection writes; background synchronization depends on compositor
capabilities. Transfers are bounded, generation tracked and kept out of logs.
Primary selection is separate.

## Notifications

A SystemUI listener forwards Android notifications to the presenter's desktop
`org.freedesktop.Notifications` D-Bus client. Application name, title, body, icon
and urgency are mapped where supported. Default clicks and supported actions
invoke Android PendingIntents. Explicit desktop dismissals cancel dismissible
Android notifications; popup expiry does not cancel them.

Reconnects reconcile active notifications. No extra persistent host daemon or
compositor extension is needed. Inline replies and custom Android layouts are
outside the current mapping.

## Diagnostics

As the desktop user, without sudo:

```console
droidloomctl crashes
droidloomctl crashes com.whatsapp
droidloomctl logs com.whatsapp -n 500
droidloomctl logs -n 1000
droidloomctl --json crashes > crash-report.json
```

These read snapshots without starting or restarting Android, including during
incomplete boot. Package logs use Android UIDs, so shared-UID packages share logs.
Crash reports include recorded exits and the global crash buffer; ordinary exits
can appear too. Output is bounded and truncation is marked. Save reports before
restarting Android, which clears logcat buffers.

For read-only host prerequisite diagnosis:

```console
cargo run --locked -j 1 -p droidloom-doctor -- --format json --pretty
```

The doctor reads kernel/runtime metadata without opening DRM or Binder devices,
creating namespaces, mounting filesystems or changing host state. Exit status is
0 for satisfied prerequisites, 2 for missing or unproven requirements, and 1 for
a tool failure. It does not prove that an app will render correctly.

Launch diagnostics include `droidloom-launch-timing` from the Android launcher
and `droidloom-launch-host-timing` from the supervisor. They measure preparation,
activity discovery and registration, not click-to-first-frame latency. Android
policy setup is cached per user and framework process lifetime; framework
restart, PID reuse or incomplete setup invalidates the cache.

## Current limitations

The unified pacman build targets x86_64 with AMD/Intel graphics. NVIDIA rendering,
ARM-only APK compatibility and unified ARM64 packaging are not established
features. There is no dedicated APK installation CLI yet.

For apps needing explicit activity selection, use
`droidloomctl launch PACKAGE --component PACKAGE/ACTIVITY`.
Clipboard support depends on compositor and receiving-app capabilities; text
input does not export surrounding text or cursor geometry. Shared-kernel
isolation is described in the [security requirements](threat-model-v1.md).
