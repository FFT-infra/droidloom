//! Primitive-only JNI exports. The opaque VM pointers are never dereferenced.

use std::ffi::c_void;
use std::sync::Mutex;

use crate::Policy;

static POLICY: Mutex<Option<Policy>> = Mutex::new(None);

// Keep build compatibility metadata in the final exported JNI library, including
// stripped Android builds. The updater verifies this alongside the Java marker.
#[unsafe(no_mangle)]
pub static DROIDLOOM_WINDOW_POLICY_ABI: [u8; 30] = *b"DROIDLOOM_WINDOW_POLICY_ABI=1;";

fn with_policy<T>(f: impl FnOnce(&mut Policy) -> T) -> T {
    let mut state = POLICY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(state.get_or_insert_with(Policy::default))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_android_server_policy_DroidloomPolicyNative_version(
    _env: *mut c_void,
    _class: *mut c_void,
) -> i32 {
    crate::ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_android_server_policy_DroidloomPolicyNative_command(
    _env: *mut c_void,
    _class: *mut c_void,
    op: i32,
    display: i32,
    generation: i32,
    value: u8,
) -> i32 {
    with_policy(|p| p.command(op, display, generation, value != 0))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_android_server_policy_DroidloomPolicyNative_query(
    _env: *mut c_void,
    _class: *mut c_void,
    op: i32,
    display: i32,
    value: u8,
) -> u8 {
    u8::from(with_policy(|p| p.query(op, display, value != 0)))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_android_server_policy_DroidloomPolicyNative_authorizeWindow(
    _env: *mut c_void,
    _class: *mut c_void,
    kind: i32,
    facts: i32,
    target_sdk: i32,
    mode: i32,
) -> i32 {
    crate::authorize_window(kind, facts, target_sdk, mode)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_android_server_policy_DroidloomPolicyNative_routeKey(
    _env: *mut c_void,
    _class: *mut c_void,
    code: i32,
    interactive: u8,
    injected: u8,
    unhandled: u8,
) -> i32 {
    crate::route_key(code, interactive != 0, injected != 0, unhandled != 0)
}
