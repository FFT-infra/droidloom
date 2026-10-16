// Keep the Android rlib independent of the libc Rust crate's linkage variant.
// Its callers include both a Rust dylib and a C++ executable with static Rust.
#[cfg(not(target_os = "android"))]
pub(crate) use libc::{
    CLOCK_MONOTONIC, CLOCK_THREAD_CPUTIME_ID, clock_gettime, clockid_t as ClockId,
    timespec as Timespec,
};

#[cfg(target_os = "android")]
pub(crate) use bionic::*;

#[cfg(target_os = "android")]
mod bionic {
    use core::ffi::{c_int, c_long};

    // Droidloom's Android products are x86_64 and AArch64. Match the pinned
    // bionic bits/timespec.h (time_t, long) and linux/time.h clock constants.
    const _: () = assert!(
        size_of::<c_long>() == 8,
        "64-bit bionic timing ABI required"
    );
    pub(crate) type ClockId = c_int;
    pub(crate) const CLOCK_MONOTONIC: ClockId = 1;
    pub(crate) const CLOCK_THREAD_CPUTIME_ID: ClockId = 3;
    #[repr(C)]
    pub(crate) struct Timespec {
        pub(crate) tv_sec: c_long,
        pub(crate) tv_nsec: c_long,
    }
    unsafe extern "C" {
        pub(crate) fn clock_gettime(clock: ClockId, time: *mut Timespec) -> c_int;
    }
}
