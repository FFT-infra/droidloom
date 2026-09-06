# Task-scoped Android input

`0001-application-token-targeted-injection.patch` extends InputDispatcher's
existing targeted-injection state with an optional input-application token.
For pointer DOWN it runs the normal touch hit test while excluding windows from
other application tokens; the resulting touch state remains latched for the
gesture. Keys select the frontmost eligible window belonging to that token
instead of consulting Android's global focused task. Targeted streams exclude
spies, outside-touch listeners, and wallpaper duplication so input cannot leak
to a different Android task.

This is downstream of Denial's ordinary window hit testing. The patch contains
no Denial policy and never changes Android task focus or stacking.

`0002-droidloom-targeted-injection-binder.patch` adds the root/system-only
native endpoint used by `droidloom-input-bridge`. It preserves the existing
`InputDispatcherInterface` ABI, parses the standard Android event parcel, and
passes the resolved application token into the concrete dispatcher path.
