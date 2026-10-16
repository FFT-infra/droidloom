//! Android `WindowManager` policy decisions, called in-process from `system_server`.
//!
//! Java supplies Android permission facts and performs Binder/framework effects.
//! The policy engine owns authorization, key routing and screen readiness without
//! host I/O. Its separate task backend reuses Composer's authenticated socket for
//! lifecycle publication and direct host window commands. Both run in-process.

#![deny(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

// Exporting JNI symbols is the only unsafe-code allowance. No JNI pointers are
// dereferenced: every argument and result is a primitive value.
#[allow(unsafe_code)]
mod jni;

#[allow(unsafe_code)]
mod framework_ffi;
mod input;
#[allow(unsafe_code)]
mod input_ffi;
mod input_keys;
#[allow(unsafe_code)]
mod task_ffi;
mod tasks;

/// Version shared with the Java adapter. A mismatched pair must not start WMS.
pub const ABI_VERSION: i32 = 1;

/// Commands accepted by the native state machine.
pub mod command {
    pub const SCREEN_START: i32 = 1;
    pub const KEYGUARD_DRAWN: i32 = 2;
    pub const WINDOWS_DRAWN: i32 = 3;
    pub const SCREEN_OFF: i32 = 4;
    pub const SYSTEM_READY: i32 = 5;
    pub const SYSTEM_BOOTED: i32 = 6;
    pub const GOING_TO_SLEEP: i32 = 7;
    pub const BOOT_WITH_OUTPUT_OFF: i32 = 8;
    pub const ALLOW_LOCKSCREEN_WHEN_ON: i32 = 9;
    pub const CLEAR_LOCKSCREEN_DISPLAYS: i32 = 10;
}

/// Queries accepted by the native state machine.
pub mod query {
    pub const SCREEN_ON: i32 = 1;
    pub const CAN_ANIMATE: i32 = 2;
    pub const BOOT_ANIMATION_DISMISSIBLE: i32 = 3;
    pub const KEYGUARD_DRAWN_ONCE: i32 = 4;
    pub const SYSTEM_READY: i32 = 5;
    pub const SYSTEM_BOOTED: i32 = 6;
    pub const LOCKSCREEN_TIMER_REQUIRED: i32 = 7;
}

/// Side effects which the Java adapter must execute outside native locks.
pub mod action {
    pub const NONE: i32 = 0;
    pub const WAIT_FOR_WINDOWS: i32 = 1;
    pub const COMPLETE_SCREEN: i32 = 2;
    pub const NOTIFY_BOOT_COMPLETED: i32 = 3;
}

#[derive(Default)]
struct Screen {
    generation: i32,
    keyguard_drawn: bool,
    windows_wait_started: bool,
    complete: bool,
}

/// Per-system_server policy state. Screen-off invalidates pending callbacks.
#[derive(Default)]
// Readiness, boot, power transitions and draw history are independent facts.
#[allow(clippy::struct_excessive_bools)]
pub struct Policy {
    screens: BTreeMap<i32, Screen>,
    lockscreen_displays: BTreeSet<i32>,
    generation: i32,
    ready: bool,
    booted: bool,
    going_to_sleep: bool,
    keyguard_drawn_once: bool,
    boot_with_output_off: bool,
    boot_callback_sent: bool,
}

impl Policy {
    /// Apply a lifecycle event. A screen-start result is a positive generation;
    /// other results are actions. Unknown commands and stale callbacks do nothing.
    pub fn command(&mut self, op: i32, display: i32, generation: i32, value: bool) -> i32 {
        match op {
            command::SCREEN_START if display >= 0 => {
                let Some(next) = self.generation.checked_add(1) else {
                    return action::NONE;
                };
                self.generation = next;
                if display == 0 {
                    self.boot_with_output_off = false;
                }
                self.screens.insert(
                    display,
                    Screen {
                        generation: next,
                        // Only the built-in display has an Android keyguard.
                        keyguard_drawn: display != 0,
                        windows_wait_started: display != 0,
                        complete: false,
                    },
                );
                next
            }
            command::KEYGUARD_DRAWN => {
                let Some(screen) = self.pending(display, generation) else {
                    return action::NONE;
                };
                if screen.windows_wait_started {
                    return action::NONE;
                }
                screen.keyguard_drawn = true;
                screen.windows_wait_started = true;
                action::WAIT_FOR_WINDOWS
            }
            command::WINDOWS_DRAWN => {
                let Some(screen) = self.pending(display, generation) else {
                    return action::NONE;
                };
                if !screen.keyguard_drawn || !screen.windows_wait_started {
                    return action::NONE;
                }
                screen.complete = true;
                if display == 0 {
                    self.keyguard_drawn_once = true;
                }
                action::COMPLETE_SCREEN
            }
            command::SCREEN_OFF => {
                self.screens.remove(&display);
                action::NONE
            }
            command::SYSTEM_READY => {
                self.ready = true;
                self.boot_notification()
            }
            command::SYSTEM_BOOTED => {
                self.booted = true;
                self.boot_notification()
            }
            command::GOING_TO_SLEEP => {
                self.going_to_sleep = value;
                action::NONE
            }
            command::BOOT_WITH_OUTPUT_OFF => {
                self.boot_with_output_off = self.booted && !self.screens.contains_key(&0);
                action::NONE
            }
            command::ALLOW_LOCKSCREEN_WHEN_ON if display >= 0 => {
                if value {
                    self.lockscreen_displays.insert(display);
                } else {
                    self.lockscreen_displays.remove(&display);
                }
                action::NONE
            }
            command::CLEAR_LOCKSCREEN_DISPLAYS => {
                self.lockscreen_displays.clear();
                action::NONE
            }
            _ => action::NONE,
        }
    }

    fn boot_notification(&mut self) -> i32 {
        if self.ready && self.booted && !self.boot_callback_sent {
            self.boot_callback_sent = true;
            action::NOTIFY_BOOT_COMPLETED
        } else {
            action::NONE
        }
    }

    fn pending(&mut self, display: i32, generation: i32) -> Option<&mut Screen> {
        self.screens
            .get_mut(&display)
            .filter(|screen| screen.generation == generation && !screen.complete)
    }

    /// Read policy state. `value` is `ignoreScreenOn` for `CAN_ANIMATE`.
    #[must_use]
    pub fn query(&self, op: i32, display: i32, value: bool) -> bool {
        match op {
            query::SCREEN_ON => self.screens.contains_key(&display),
            query::CAN_ANIMATE => (value || self.screens.contains_key(&0)) && !self.going_to_sleep,
            query::BOOT_ANIMATION_DISMISSIBLE => {
                self.screens
                    .get(&0)
                    .is_some_and(|screen| screen.keyguard_drawn)
                    || self.boot_with_output_off
            }
            query::KEYGUARD_DRAWN_ONCE => self.keyguard_drawn_once,
            query::SYSTEM_READY => self.ready,
            query::SYSTEM_BOOTED => self.booted,
            query::LOCKSCREEN_TIMER_REQUIRED => !self.lockscreen_displays.is_empty() && value,
            _ => false,
        }
    }
}

/// Window classification supplied by Android's `LayoutParams` constants.
pub mod window {
    pub const INVALID: i32 = 0;
    pub const APPLICATION: i32 = 1;
    pub const TOAST: i32 = 2;
    pub const TOKEN_CHECKED: i32 = 3;
    pub const INTERNAL: i32 = 4;
    pub const ALERT: i32 = 5;
    pub const APPLICATION_OVERLAY: i32 = 6;
}

/// Android permission facts, collected with the original Binder caller identity.
pub mod permission {
    pub const ROUNDED_OVERLAY: i32 = 1;
    pub const INTERNAL_WINDOW: i32 = 1 << 1;
    pub const SYSTEM_UID: i32 = 1 << 2;
    pub const PACKAGE_CHECKED: i32 = 1 << 3;
    pub const PACKAGE_EXISTS: i32 = 1 << 4;
    pub const SYSTEM_OVERLAY: i32 = 1 << 5;
    pub const TRUSTED_VIRTUAL_OWNER: i32 = 1 << 6;
    pub const ALERT_WINDOW: i32 = 1 << 7;
}

/// Authorization result; NEED_* asks the adapter for additional platform facts.
pub mod authorization {
    pub const ALLOW: i32 = 0;
    pub const DENY: i32 = 1;
    pub const INVALID_TYPE: i32 = 2;
    pub const NEED_PACKAGE: i32 = 3;
    pub const NEED_APP_OP: i32 = 4;
}

/// Evaluate Android window authorization, including legacy overlay behavior.
/// `mode` uses `AppOpsManager`'s MODE_* values; -1 means it has not been noted.
/// ALLOW for `MODE_IGNORED` preserves WMS's subsequent hidden-window handling.
#[must_use]
pub fn authorize_window(kind: i32, facts: i32, target_sdk: i32, mode: i32) -> i32 {
    use authorization::{ALLOW, DENY, INVALID_TYPE, NEED_APP_OP, NEED_PACKAGE};
    use permission as p;
    let has = |flag| facts & flag != 0;
    if has(p::ROUNDED_OVERLAY) && !has(p::INTERNAL_WINDOW) {
        return DENY;
    }
    match kind {
        window::INVALID => return INVALID_TYPE,
        window::APPLICATION | window::TOAST | window::TOKEN_CHECKED => return ALLOW,
        window::INTERNAL => return if has(p::INTERNAL_WINDOW) { ALLOW } else { DENY },
        window::ALERT | window::APPLICATION_OVERLAY => {}
        _ => return DENY,
    }
    if has(p::SYSTEM_UID) {
        return ALLOW;
    }
    if !has(p::PACKAGE_CHECKED) {
        return NEED_PACKAGE;
    }
    if !has(p::PACKAGE_EXISTS) || (kind != window::APPLICATION_OVERLAY && target_sdk >= 26) {
        return if has(p::INTERNAL_WINDOW) { ALLOW } else { DENY };
    }
    if has(p::SYSTEM_OVERLAY) || has(p::TRUSTED_VIRTUAL_OWNER) {
        return ALLOW;
    }
    match mode {
        -1 => NEED_APP_OP,
        0 | 1 => ALLOW, // MODE_ALLOWED / MODE_IGNORED (WMS hides ignored overlays)
        2 if target_sdk < 23 => ALLOW, // MODE_ERRORED: don't crash pre-M apps
        2 => DENY,
        _ => {
            if has(p::ALERT_WINDOW) {
                ALLOW
            } else {
                DENY
            }
        }
    }
}

/// Key routes understood by the Java adapter.
pub mod key {
    pub const CONSUME: i32 = 0;
    pub const APPLICATION: i32 = 1;
    pub const MEDIA: i32 = 2;
    pub const VOLUME: i32 = 3;
}

/// Host-owned system keys must never trigger Android's phone shell or power UI.
#[must_use]
pub fn host_key(code: i32) -> bool {
    matches!(code, 3 | 26 | 187 | 220 | 221 | 223 | 224 | 276 | 280..=284)
}

/// Choose a key destination. Back, IME, ordinary shortcuts and accessibility
/// input keep Android dispatch semantics. Host output state owns wake/sleep.
#[must_use]
pub fn route_key(code: i32, interactive: bool, injected: bool, unhandled: bool) -> i32 {
    if host_key(code) {
        return key::CONSUME;
    }
    if !unhandled && (interactive || injected) {
        return key::APPLICATION;
    }
    match code {
        24 | 25 | 164 => key::VOLUME,
        79 | 85..=91 | 126..=130 | 222 | 272..=275 => key::MEDIA,
        _ => key::CONSUME,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_waits_for_keyguard_then_windows_and_completes_once() {
        let mut p = Policy::default();
        let epoch = p.command(command::SCREEN_START, 0, 0, false);
        assert!(epoch > 0);
        assert!(p.query(query::SCREEN_ON, 0, false));
        assert_eq!(
            p.command(command::WINDOWS_DRAWN, 0, epoch, false),
            action::NONE
        );
        assert!(!p.query(query::BOOT_ANIMATION_DISMISSIBLE, 0, false));
        assert_eq!(
            p.command(command::KEYGUARD_DRAWN, 0, epoch, false),
            action::WAIT_FOR_WINDOWS
        );
        assert_eq!(
            p.command(command::KEYGUARD_DRAWN, 0, epoch, false),
            action::NONE
        );
        assert!(p.query(query::BOOT_ANIMATION_DISMISSIBLE, 0, false));
        assert_eq!(
            p.command(command::WINDOWS_DRAWN, 0, epoch, false),
            action::COMPLETE_SCREEN
        );
        assert_eq!(
            p.command(command::WINDOWS_DRAWN, 0, epoch, false),
            action::NONE
        );
        assert!(p.query(query::KEYGUARD_DRAWN_ONCE, 0, false));
    }

    #[test]
    fn sleep_wake_and_display_reuse_reject_stale_completions() {
        let mut p = Policy::default();
        let old = p.command(command::SCREEN_START, 0, 0, false);
        p.command(command::KEYGUARD_DRAWN, 0, old, false);
        p.command(command::SCREEN_OFF, 0, 0, false);
        let new = p.command(command::SCREEN_START, 0, 0, false);
        assert_ne!(old, new);
        for op in [command::KEYGUARD_DRAWN, command::WINDOWS_DRAWN] {
            assert_eq!(p.command(op, 0, old, false), action::NONE);
        }
        assert!(!p.query(query::KEYGUARD_DRAWN_ONCE, 0, false));
        p.command(command::KEYGUARD_DRAWN, 0, new, false);
        assert_eq!(
            p.command(command::WINDOWS_DRAWN, 0, new, false),
            action::COMPLETE_SCREEN
        );
    }

    #[test]
    fn secondary_displays_are_independent_and_do_not_need_keyguard() {
        let mut p = Policy::default();
        let default = p.command(command::SCREEN_START, 0, 0, false);
        let other = p.command(command::SCREEN_START, 7, 0, false);
        assert_eq!(
            p.command(command::WINDOWS_DRAWN, 7, default, false),
            action::NONE
        );
        assert_eq!(
            p.command(command::WINDOWS_DRAWN, 7, other, false),
            action::COMPLETE_SCREEN
        );
        assert!(!p.query(query::KEYGUARD_DRAWN_ONCE, 0, false));
        assert!(!p.query(query::BOOT_ANIMATION_DISMISSIBLE, 0, false));
        p.command(command::SCREEN_OFF, 7, 0, false);
        assert!(p.query(query::SCREEN_ON, 0, false));
        assert!(!p.query(query::SCREEN_ON, 7, false));
    }

    #[test]
    fn boot_with_outputs_off_does_not_wait_forever() {
        let mut p = Policy::default();
        assert!(!p.query(query::BOOT_ANIMATION_DISMISSIBLE, 0, false));
        p.command(command::SYSTEM_BOOTED, 0, 0, false);
        assert!(!p.query(query::BOOT_ANIMATION_DISMISSIBLE, 0, false));
        p.command(command::BOOT_WITH_OUTPUT_OFF, 0, 0, false);
        assert!(p.query(query::BOOT_ANIMATION_DISMISSIBLE, 0, false));
        p.command(command::SCREEN_START, 0, 0, false);
        assert!(!p.query(query::BOOT_ANIMATION_DISMISSIBLE, 0, false));
        p.command(command::SCREEN_OFF, 0, 0, false);
        assert!(!p.query(query::CAN_ANIMATE, 0, false));
        assert!(p.query(query::CAN_ANIMATE, 0, true));
        p.command(command::GOING_TO_SLEEP, 0, 0, true);
        assert!(!p.query(query::CAN_ANIMATE, 0, true));
        p.command(command::GOING_TO_SLEEP, 0, 0, false);
        assert!(p.query(query::CAN_ANIMATE, 0, true));
    }

    #[test]
    fn boot_callback_is_emitted_once_in_either_lifecycle_order() {
        for events in [
            [command::SYSTEM_READY, command::SYSTEM_BOOTED],
            [command::SYSTEM_BOOTED, command::SYSTEM_READY],
        ] {
            let mut p = Policy::default();
            assert_eq!(p.command(events[0], 0, 0, false), action::NONE);
            assert_eq!(
                p.command(events[1], 0, 0, false),
                action::NOTIFY_BOOT_COMPLETED
            );
            for op in events {
                assert_eq!(p.command(op, 0, 0, false), action::NONE);
            }
        }
    }

    #[test]
    fn screen_on_lock_policy_requires_a_secure_awake_user_and_resets_on_switch() {
        let mut p = Policy::default();
        assert!(!p.query(query::LOCKSCREEN_TIMER_REQUIRED, 0, true));
        p.command(command::ALLOW_LOCKSCREEN_WHEN_ON, 2, 0, true);
        assert!(p.query(query::LOCKSCREEN_TIMER_REQUIRED, 0, true));
        assert!(!p.query(query::LOCKSCREEN_TIMER_REQUIRED, 0, false));
        p.command(command::ALLOW_LOCKSCREEN_WHEN_ON, 2, 0, false);
        assert!(!p.query(query::LOCKSCREEN_TIMER_REQUIRED, 0, true));
        p.command(command::ALLOW_LOCKSCREEN_WHEN_ON, 0, 0, true);
        p.command(command::CLEAR_LOCKSCREEN_DISPLAYS, 0, 0, false);
        assert!(!p.query(query::LOCKSCREEN_TIMER_REQUIRED, 0, true));
    }

    #[test]
    fn generations_never_wrap_into_a_stale_callback() {
        let mut p = Policy {
            generation: i32::MAX,
            ..Policy::default()
        };
        assert_eq!(p.command(command::SCREEN_START, 0, 0, false), 0);
        assert!(!p.query(query::SCREEN_ON, 0, false));
        assert_eq!(p.command(999, 0, 0, false), 0);
        assert!(!p.query(999, 0, false));
    }

    #[test]
    fn rounded_corners_and_internal_windows_require_signature_permission() {
        use authorization::*;
        use permission as f;
        for kind in [
            window::APPLICATION,
            window::TOAST,
            window::TOKEN_CHECKED,
            window::INTERNAL,
        ] {
            assert_eq!(authorize_window(kind, f::ROUNDED_OVERLAY, 0, -1), DENY);
            assert_eq!(
                authorize_window(kind, f::ROUNDED_OVERLAY | f::INTERNAL_WINDOW, 0, -1),
                ALLOW
            );
        }
        assert_eq!(authorize_window(window::INTERNAL, 0, 0, -1), DENY);
        assert_eq!(authorize_window(window::INVALID, 0, 0, -1), INVALID_TYPE);
    }

    #[test]
    fn modern_apps_cannot_use_legacy_alert_types() {
        use authorization::*;
        use permission as f;
        let known = f::PACKAGE_CHECKED | f::PACKAGE_EXISTS;
        assert_eq!(authorize_window(window::ALERT, 0, -1, -1), NEED_PACKAGE);
        assert_eq!(authorize_window(window::ALERT, known, 26, 0), DENY);
        assert_eq!(
            authorize_window(window::ALERT, known | f::SYSTEM_OVERLAY, 26, 0),
            DENY
        );
        assert_eq!(
            authorize_window(window::ALERT, known | f::INTERNAL_WINDOW, 26, 0),
            ALLOW
        );
        assert_eq!(
            authorize_window(window::APPLICATION_OVERLAY, f::PACKAGE_CHECKED, -1, 0),
            DENY
        );
        assert_eq!(
            authorize_window(window::ALERT, f::SYSTEM_UID, -1, -1),
            ALLOW
        );
    }

    #[test]
    fn overlay_appops_preserve_modern_and_legacy_semantics() {
        use authorization::*;
        use permission as f;
        let known = f::PACKAGE_CHECKED | f::PACKAGE_EXISTS;
        for sdk in [22, 23, 25, 26, 37] {
            for kind in [window::ALERT, window::APPLICATION_OVERLAY] {
                if kind == window::ALERT && sdk >= 26 {
                    continue;
                }
                assert_eq!(authorize_window(kind, known, sdk, -1), NEED_APP_OP);
                for mode in [0, 1] {
                    assert_eq!(authorize_window(kind, known, sdk, mode), ALLOW);
                }
                assert_eq!(
                    authorize_window(kind, known, sdk, 2),
                    if sdk < 23 { ALLOW } else { DENY }
                );
                assert_eq!(authorize_window(kind, known, sdk, 3), DENY);
                assert_eq!(
                    authorize_window(kind, known | f::ALERT_WINDOW, sdk, 3),
                    ALLOW
                );
            }
        }
        for special in [f::SYSTEM_OVERLAY, f::TRUSTED_VIRTUAL_OWNER] {
            assert_eq!(
                authorize_window(window::APPLICATION_OVERLAY, known | special, 37, -1),
                ALLOW
            );
        }
    }

    #[test]
    fn android_keys_and_injected_input_keep_normal_dispatch() {
        for code in [4, 29, 66, 67, 111] {
            // Back, letters, Enter, Delete, Escape
            assert_eq!(route_key(code, true, false, false), key::APPLICATION);
            assert_eq!(route_key(code, false, true, false), key::APPLICATION);
            assert_eq!(route_key(code, false, false, false), key::CONSUME);
        }
        for code in [3, 26, 187, 220, 221, 223, 224, 276, 280, 284] {
            for interactive in [false, true] {
                assert_eq!(route_key(code, interactive, true, false), key::CONSUME);
            }
        }
        assert_eq!(route_key(85, false, false, false), key::MEDIA);
        assert_eq!(route_key(24, true, false, false), key::APPLICATION);
        assert_eq!(route_key(24, true, false, true), key::VOLUME);
    }
}
