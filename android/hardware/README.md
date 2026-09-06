# Android hardware integration boundary

Android Binder service contracts and HAL build integration live here:

- Composer AIDL/HWC3 transport to the private host-presenter socket;
- GBM/DMA-BUF allocator and mapper;
- supporting Android HAL configuration. Dedicated sensor, vibrator and camera
  brokers are not established preview features.

The Composer path must use the protocol contract in `protocol/` and may never
open a DRM card/KMS node.

The Composer state machine is implemented in safe Rust at
graphics/droidloom-composer. The exact minigbm handle parser lives in
graphics/droidloom-minigbm, and the Binder-only slot/FD adapter lives in
graphics/droidloom-composer-aidl. The Denial codec and audited Unix descriptor
transport live in graphics/droidloom-denial-protocol and
graphics/droidloom-denial-ipc. composer/aidl-lock.json pins the frozen Android
interfaces, while composer/service-contract.json records the
syncobj presentation-sink and service-registration boundary inside the Android
vendor image.
