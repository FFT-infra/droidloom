//! Compile the narrow EGL/GLES ABI shim used by the Rust probe.

fn main() {
    cc::Build::new()
        .file("src/egl_renderer.c")
        .flag_if_supported("-std=c11")
        .warnings(true)
        .compile("droidloom_egl_renderer");
    println!("cargo:rustc-link-lib=EGL");
    println!("cargo:rustc-link-lib=GLESv2");
    println!("cargo:rerun-if-changed=src/egl_renderer.c");
}
