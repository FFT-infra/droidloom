// Run this isolated crate with `cargo test --locked --manifest-path
// graphics/droidloom-composer-service/test-harness/Cargo.toml`. It compiles the
// production writer module with a minimal harness so queue-order and socket
// backpressure tests do not need the host compositor or GPU runtime.
#![allow(dead_code)]

#[path = "../../src/input.rs"]
mod input;
