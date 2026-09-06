# Host text input

Droidloom uses the compositor's ordinary `zwp_text_input_v3` interface on the
same seat and surfaces as its Android task windows. Denial needs no changes.

Android's headless `com.android.droidloom.ime/.HostInputMethod` observes
`onStartInput`, `onFinishInput`, and explicit input-panel requests. It never
draws an Android keyboard or requests fullscreen IME mode. Android init installs
the packaged APK when its hash changes, enables/selects the method, and enables
IME requests with a hardware keyboard once after boot. No per-frame settings
reads or editor polling are used.

Panel dismissal is separate from an editable input connection. The headless
method handles `hideWindow` directly: Android does not call `onWindowHidden`
when no Android keyboard window was visible. A subsequent `restartInput` must
preserve dismissal even if `EditorInfo.inputType` remains editable. An explicit
show request or a genuinely new editor can re-enable it. `EditorVisibilityTest`
covers hide-then-restart behavior on the host JVM.

The privileged framework bridge authenticates the IME's Unix peer UID against
PackageManager, resolves its focused Android task, and relays bounded JSON
records to the presenter's private `text-input.sock`. The supervisor exposes
only that socket inside the cell. The host accepts only UID 0 bridge peers.
Neither connection logs editor contents. Editor state contains only session,
task, input type, enablement, and a monotonically increasing show-request counter;
surrounding text is not exported. Show requests do not create a new editor
session: repeated `showSoftInput` refreshes content state with a Wayland commit,
not another `enable`. This preserves compositor touch authorization and permits
reopening a user-dismissed panel without invalidating the input connection.

The presenter enables text input only when Android's task, the host keyboard
focus, and Wayland's text-input enter surface agree. It forwards committed
transactions only for the current Wayland commit serial and Android editor
session. Focus loss, connection loss, and task destruction disable the session;
text-bridge failures do not terminate graphics or ordinary key injection.

Committed text and preedit return through Android's `InputConnection`.
UTF-8 deletion lengths are converted to UTF-16 without splitting code points.
Password fields advertise sensitive/hidden text hints. This first bridge does
not export surrounding text, cursor rectangles, selection, or editor-action
metadata; full prediction/context and precise IME candidate placement remain
future work. Physical key delivery remains on the existing routed-key path.

The [Rust package builder](../../packaging/arch/README.md) builds the IME,
framework bridge, task launcher and Composer endpoint together. Task registration
is idempotent for restored and newly created activities.

## Activity routing

Activity discovery canonicalizes launcher aliases through PackageManager's
`ActivityInfo.targetActivity`, and accepts an alias only when its target matches
the completed `start-activity -W` result. This matters for both Settings and
Firefox; treating a real activity and its launcher alias as different tasks
strands the running app. No-display trampoline successors remain distinct.

Presenter activation logs include `touch_age_ms`; IME logs distinguish
start/restart, finish, and show callbacks without logging editor text.

Denial uses focus-scoped touch authorization for keyboard activation and revokes
it on shell touches. Activation is counted when an editor consumes authorization,
not on raw protocol-client touches.

## Host keyboard dismissal

The optional `denial_text_input_panel_manager_v1` Wayland extension associates
dismissal feedback with our ordinary text-input-v3 object. Denial sends only a
completed user dismissal, identified by the text input's commit serial. The
presenter ignores stale and duplicate feedback and relays a bounded JSON record
containing `dismiss: true`, `session`, and `show_request` to the headless IME.
The IME checks both identities, records hidden state, finishes composition, and
calls Android's `requestHideSelf(0)`. That informs Android and the app instead of
faking Back/Done or forcing view focus loss. A later tap on the same focused
editor can request the keyboard again; actual view-focus behavior belongs to
the application.

The protocol XML is vendored in `graphics/droidloom-wayland/protocol/` and must
match Denial's copy in `compositor/protocol/`. No Denial checkout is needed to
build the presenter. Absence of the extension leaves ordinary text input usable
but cannot report manual host dismissal to Android. The extension has no Android-specific wire semantics.

Tests cover dismissal serials, duplicate events, stale Android show requests,
hide/restart preservation, and reopening the same editor.
