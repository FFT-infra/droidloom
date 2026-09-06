# Android source boundary

Droidloom uses a thin AOSP product. The full upstream checkout and generated
images are not stored in this repository.

`manifest/source-lock.json` pins ARM64 base inputs;
`manifest/source-lock-x86_64.json` pins x86_64 inputs. The sparse AOSP project
closure is recorded separately in `manifest/m2-sparse-source-lock.json`.

The [Rust package workflow](../packaging/arch/README.md) prepares the pinned
upstream base partitions, materializes required sources, applies Droidloom patches,
builds modified platform components and the vendor image, and assembles packages.
Mesa is prepared from its pinned upstream release. It does not rebuild all of AOSP.

Cuttlefish vendor, ODM, boot and kernel images are excluded: their virtual-device
graphics and KMS assumptions do not match Droidloom. The host retains its kernel.

- [Manifest](manifest/README.md): source locks and sparse materialization.
- [Device products](device/README.md): architecture configuration and image boundary.
- [Framework](framework/README.md): Android adapters and minimal system apps.
- [Hardware](hardware/README.md): Composer and allocator integration.
- [Mesa](mesa/README.md): graphics source preparation.
