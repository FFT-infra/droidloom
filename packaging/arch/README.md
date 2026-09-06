# Droidloom pacman packaging

User-facing instructions are maintained in:

- [Installation and APKs](../../docs/INSTALL.md)
- [Building packages](../../docs/BUILDING.md)
- [Known issues](../../docs/KNOWN_ISSUES.md)

`version.json` defines the matching package release. `PKGBUILD.in` calls the Rust
builder, and `Containerfile` supplies its rootless Arch environment. The runtime
and image packages must be installed together. Lifecycle hooks stop the runtime
before replacement or removal and preserve Android data.

Both packages include Droidloom's GPL notice and license text under
`/usr/share/licenses/<package>/`. The runtime includes Cargo dependency notices;
the image includes Android and Mesa notices. See [THIRD_PARTY.md](../../THIRD_PARTY.md)
for license scope and corresponding-source requirements.
