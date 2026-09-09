# AOSP products

This directory defines ARM64 and x86_64 vendor products, BoardConfig, VINTF
manifests, init files, overlays and package allowlists. Builds use pinned inputs
from `android/manifest/`.

`droidloom-arm64-product.json` and `droidloom-x86_64-product.json` describe
the assembly boundary: reusable upstream base partitions plus Droidloom's vendor
partition. Cuttlefish kernel, vendor and graphics inputs are excluded.

`droidloom_arm64/` and `droidloom_x86_64/` contain the authoritative product
configuration. Neither builds an Android-owned kernel. The root `device/`
directories supply conventional board-discovery entry points when this repository
is projected at `vendor/droidloom` in the AOSP tree.

The x86_64 product supplies AMD/Intel graphics for the current pacman preview.
Its ARM64 NativeBridge target compiles guest libraries alongside the native
x86_64 platform. The updater installs that closure in a derived system image;
building only `vendorimage` does not activate translation.
ARM64 retains a specialist build/deployment workflow. See
[current limitations](../../docs/desktop-integration.md#current-limitations); product definitions
alone do not establish hardware compatibility.
