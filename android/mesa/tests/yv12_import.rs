//! Offscreen Android allocator/EGL import regression. Does not open a window.
use std::{
    ffi::{c_char, c_void},
    ptr,
};
unsafe extern "C" {
    fn dlopen(name: *const c_char, flags: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}
macro_rules! symbol {
    ($lib:expr, $name:literal, $ty:ty) => {{
        let p = dlsym($lib, concat!($name, "\0").as_ptr().cast());
        assert!(!p.is_null(), "missing {}", $name);
        std::mem::transmute::<*mut c_void, $ty>(p)
    }};
}
#[repr(C)]
#[derive(Default)]
struct Desc {
    width: u32,
    height: u32,
    layers: u32,
    format: u32,
    usage: u64,
    stride: u32,
    rfu0: u32,
    rfu1: u64,
}
fn main() {
    // SAFETY: signatures and layout match Android NDK and EGL declarations;
    // owned hardware buffers and images remain live until explicitly released.
    unsafe {
        let native = dlopen(c"libnativewindow.so".as_ptr(), 2);
        let egl = dlopen(c"libEGL.so".as_ptr(), 2);
        assert!(!native.is_null() && !egl.is_null());
        let allocate = symbol!(
            native,
            "AHardwareBuffer_allocate",
            unsafe extern "C" fn(*const Desc, *mut *mut c_void) -> i32
        );
        let describe = symbol!(
            native,
            "AHardwareBuffer_describe",
            unsafe extern "C" fn(*mut c_void, *mut Desc)
        );
        let release = symbol!(
            native,
            "AHardwareBuffer_release",
            unsafe extern "C" fn(*mut c_void)
        );
        let display = symbol!(
            egl,
            "eglGetDisplay",
            unsafe extern "C" fn(*mut c_void) -> *mut c_void
        )(ptr::null_mut());
        let (mut major, mut minor) = (0, 0);
        assert_ne!(
            symbol!(
                egl,
                "eglInitialize",
                unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> u32
            )(display, &mut major, &mut minor),
            0
        );
        let get_proc = symbol!(
            egl,
            "eglGetProcAddress",
            unsafe extern "C" fn(*const c_char) -> *mut c_void
        );
        let client_ptr = get_proc(c"eglGetNativeClientBufferANDROID".as_ptr());
        let create_ptr = get_proc(c"eglCreateImageKHR".as_ptr());
        let destroy_ptr = get_proc(c"eglDestroyImageKHR".as_ptr());
        assert!(!client_ptr.is_null() && !create_ptr.is_null() && !destroy_ptr.is_null());
        let client: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
            std::mem::transmute(client_ptr);
        let create: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            u32,
            *mut c_void,
            *const i32,
        ) -> *mut c_void = std::mem::transmute(create_ptr);
        let destroy: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32 =
            std::mem::transmute(destroy_ptr);
        for (width, height) in [(576, 1024), (642, 482), (1280, 720)] {
            let desc = Desc {
                width,
                height,
                layers: 1,
                format: 0x32315659,
                usage: 0x333,
                ..Desc::default()
            };
            let mut buffer = ptr::null_mut();
            assert_eq!(allocate(&desc, &mut buffer), 0, "YV12 allocation");
            let mut actual = Desc::default();
            describe(buffer, &mut actual);
            let image = create(
                display,
                ptr::null_mut(),
                0x3140,
                client(buffer),
                ptr::null(),
            );
            assert!(
                !image.is_null(),
                "YV12 {width}x{height} stride={} EGL import failed",
                actual.stride
            );
            assert_ne!(destroy(display, image), 0);
            release(buffer);
            println!(
                "PASS: YV12 {width}x{height}, luma stride={}, native EGL image imported",
                actual.stride
            );
        }
        assert_ne!(
            symbol!(
                egl,
                "eglTerminate",
                unsafe extern "C" fn(*mut c_void) -> u32
            )(display),
            0
        );
    }
}
