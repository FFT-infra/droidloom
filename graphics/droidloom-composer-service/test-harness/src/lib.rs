// Run this isolated crate with `cargo test --locked --manifest-path
// graphics/droidloom-composer-service/test-harness/Cargo.toml`. It compiles the
// production writer and texture-coordinate modules so queue/backpressure and
// crop-transform regressions do not need the host compositor or GPU runtime.
#![allow(dead_code)]

#[path = "../../src/input.rs"]
mod input;

#[path = "../../../droidloom-composer-aidl/src/texture_coordinates.rs"]
mod texture_coordinates;
