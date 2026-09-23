# Task-scoped Android input

`0001-application-token-targeted-injection.patch` extends InputDispatcher's
existing targeted-injection state with an optional input-application token.
For pointer DOWN it runs the normal touch hit test while excluding windows from
other application tokens; the resulting touch state remains latched for the
gesture. Keys select the frontmost eligible window belonging to that token
instead of consulting Android's global focused task. Targeted streams exclude
spies, outside-touch listeners, and wallpaper duplication so input cannot leak
to a different Android task.
InputDispatcher keeps touch state per display and pointers per input-device ID.
For token-targeted events, validation must inspect only windows with pointers for
the current device, including hover pointers. Otherwise a parallel raw stream
captured by the spy overlay can veto Droidloom's separate app-targeted stream.
For token-targeted stylus motion, InputDispatcher derives a reserved negative
virtual device ID from the 0–31 tablet pointer ID. Targeted touch keeps the
ordinary injected device ID. This lets touch and each pen keep separate pointer
state without changing Android's input-filter policy. InputState also preserves
these independent Droidloom virtual streams in the same window when Android's
general multi-device stream flag is disabled; other device combinations retain
their normal cancellation behavior.

This is downstream of Denial's ordinary window hit testing. The patch contains
no Denial policy and never changes Android task focus or stacking.

`0002-droidloom-targeted-injection-binder.patch` adds the root/system-only
native endpoint used by `droidloom-input-bridge`. It preserves the existing
`InputDispatcherInterface` ABI, parses the standard Android event parcel, and
passes the resolved application token into the concrete dispatcher path.
