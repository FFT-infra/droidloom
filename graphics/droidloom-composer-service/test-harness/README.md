# Composer input-writer tests

Run the production input module's isolated queue, priority, and slow-peer tests
from the repository root:

```sh
cargo test --locked --manifest-path graphics/droidloom-composer-service/test-harness/Cargo.toml
```

The harness imports `../src/input.rs` directly and keeps the tests independent
of the running compositor, Android cell, and physical GPU. It verifies bounded
queue behavior, window-control priority, motion coalescing, and the framework
socket write timeout.
