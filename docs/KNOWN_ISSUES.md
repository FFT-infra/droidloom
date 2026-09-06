# Known issues and preview limits

These limitations apply to the current pacman preview. Installation and APK
commands are in [INSTALL.md](INSTALL.md); source builds are in
[BUILDING.md](BUILDING.md).

## Application compatibility

- **No ARM translation.** The packaged Android userspace supports x86_64 native
  code and apps that need no native libraries. ARM64-only and 32-bit native APKs
  are unsupported. ARM64 translation has not been demonstrated in Droidloom.
- **Fruit Ninja first launch.** The tested 2.8.9 x86_64 APK switches to an age
  screen during startup. Task discovery can fail even though Android opened that
  screen. Repeat the normal launch command once to attach its window. A
  successful CLI response alone does not prove a visible desktop window.
- **Legacy app layouts.** Revision 7 enables Android's force-resizable setting
  so non-resizable apps can enter desktop windowing mode. This does not guarantee
  that every game handles arbitrary window sizes or desktop input correctly.
- **App services.** Google Play Store / Google Play Services are not supplied as
  an installation step by this preview. Installing an APK does not establish that
  its account, billing, integrity checks or other service dependencies will work.
- **Manual APK installation.** There is no `droidloomctl install` command or
  drag-and-drop installer. The documented procedure handles standalone APKs;
  split bundles and separate game assets need additional installation work.

## Desktop and runtime

- **Startup is manual.** Run `droidloomctl start` before launching an app. App
  launch does not start a stopped runtime. First boot may take longer than later
  starts while Android initializes its data.
- **Graphics support is bounded.** The package targets AMD and Intel. A dual-GPU
  AMD/NVIDIA laptop was tested using AMD; NVIDIA rendering is not validated.
- **First-start authentication needs a desktop agent.** If the polkit prompt
  is unavailable, use the explicit sudo setup helper in the installation guide.
  The exact normal-user graphical authentication path still needs a complete
  recipient-style rehearsal; the fresh installation test used explicit setup.
- **Clipboard and input depend on the desktop and app.** Background clipboard
  sharing depends on compositor protocol support. Surrounding text and cursor
  geometry are not exported through the text-input integration. Inline
  notification replies and custom Android notification layouts are unsupported.
- **Shared host kernel.** This is an Android container runtime, not a virtual
  machine. See [security requirements](threat-model-v1.md) for isolation boundaries.

## Build and distribution

- The rootless package workflow has passed package lifecycle checks and fresh
  Omarchy installation tests, but downloading all sources with completely empty
  caches has not yet been rehearsed.
- Package staging needs additional disk space beyond retained compiler outputs.
  A build can compile successfully and still fail during image copying or archive
  creation if the filesystem is full.
- Uninstalling the packages retains Android data and personal configuration.
  Reinstalling or downgrading the software does not reset or roll back app data.
- Older developer-installed runtimes can conflict with package-owned paths.
  Back up and retire such an installation before migration; do not use blanket
  pacman overwrite options.

## Reporting a problem

Include the package revision, compositor, GPU, APK version and architecture, the
command used, and what appeared on screen. Collect logs before restarting Android:

```console
pacman -Q droidloom-runtime droidloom-image
droidloomctl logs -n 500
droidloomctl crashes com.example.app
journalctl --user -u droidloom.service -n 100 --no-pager
```

Replace `com.example.app` with the affected package. Review logs for personal
information before sharing them. A package check passing establishes installation
behavior, not compatibility with every Android app.
