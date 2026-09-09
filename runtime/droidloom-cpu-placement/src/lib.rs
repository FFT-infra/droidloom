//! Explicit worker roles with optional platform CPU placement.
//! Linux topology discovery and Android's cell-owned task groups share this API.
#![deny(unsafe_op_in_unsafe_fn)]
use std::{io, thread};
#[cfg(not(target_os = "android"))]
mod linux;
#[cfg(not(target_os = "android"))]
pub use linux::{CpuSet, Groups, command, initialize};

/// Placement relative to the CPU domain inherited at process startup.
#[derive(Clone, Copy, Debug)]
pub enum Role {
    /// Frame production, submission, presentation and their immediate dependencies.
    Graphics,
    /// Ordinary work which the scheduler may place on either class.
    Normal,
    /// Work excluded from the frame's critical path.
    Background,
}

/// Apply the declared role before a worker creates dependencies or starts work.
pub fn current(role: Role) {
    #[cfg(target_os = "android")]
    let result = {
        use std::io::Write;
        let group = match role {
            Role::Graphics => "top-app",
            Role::Normal => "foreground",
            Role::Background => "background",
        };
        // Open an existing kernel tasks file; never create a fake control file.
        // Zero identifies this thread and needs no libc/Rust ABI dependency.
        std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/dev/cpuset/{group}/tasks"))
            .and_then(|mut file| file.write_all(b"0"))
    };
    #[cfg(not(target_os = "android"))]
    let result = linux::apply_current(role);
    if let Err(error) = result {
        eprintln!("Droidloom CPU placement {role:?} failed: {error}");
    }
}

/// Spawn a named owned worker with placement applied before its first operation.
pub fn spawn<F, T>(name: &str, role: Role, work: F) -> io::Result<thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    thread::Builder::new().name(name.into()).spawn(move || {
        current(role);
        work()
    })
}
