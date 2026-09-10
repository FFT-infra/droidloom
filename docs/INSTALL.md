# Install Droidloom

This guide covers the x86_64 Arch Linux / Omarchy preview. Read
[known issues](KNOWN_ISSUES.md) before installing. To produce the packages yourself,
follow [BUILDING.md](BUILDING.md), then return here.

## 1. Check prerequisites

- An x86_64 machine running Arch Linux or Omarchy, with a Wayland desktop.
- An AMD or Intel GPU. On a dual-GPU laptop, setup selects a supported render
  node; NVIDIA rendering is not validated.
- A working systemd user session and administrator access through sudo or polkit.
- At least 20 GiB free for packages, installed files and Android app data.
  The package pair is about 975 MiB to download and 2.5 GiB installed, before apps.

Run the desktop commands below in a terminal **inside your Wayland session**, as
your ordinary user. Do not run `droidloomctl start` or app launches with sudo.

## 2. Install both packages

Obtain the matching `droidloom-runtime` and `droidloom-image` archives from the
preview distribution, or build them yourself. Put only one matching pair in a
directory, then open a terminal there:

```console
sudo pacman -U ./droidloom-runtime-*.pkg.tar.zst ./droidloom-image-*.pkg.tar.zst
```

Sudo allows pacman to install system files and dependencies and update its package
database. Both packages are required and must have the same version and release.
Installation leaves Droidloom stopped. No compilation runs on the recipient's
machine when installing prebuilt packages.

## 3. Complete first-time setup and start Android

```console
droidloomctl start
```

On first start, Droidloom requests administrator authentication to create
configuration under `/etc/droidloom`, Android data under
`/var/lib/droidloom/users/<uid>/data`, and the permission rule for managing
`droidloomd.service`. Approve the prompt in your desktop authentication agent.
Subsequent starts do not need sudo.

If the authentication prompt is unavailable, run the setup helper explicitly
from your ordinary user's terminal, then retry start:

```console
sudo /usr/lib/droidloom/droidloom-package-helper setup --uid "$(id -u)" \
  --data-home "${XDG_DATA_HOME:-$HOME/.local/share}" \
  --state-home "${XDG_STATE_HOME:-$HOME/.local/state}"
droidloomctl start
```

This sudo command performs the same privileged setup described above. First boot
can take longer while Android initializes its data. `start` and `restart` wait
for Android readiness before printing `Droidloom is running; Android is ready`.
They show the observed boot stage: Android init, boot completion (including
runtime and boot-animation service states when available), and Droidloom's input
and task services. These are readiness observations, not a percentage estimate.

Boot readiness has a 120-second deadline. While a command waits, progress is
repeated every 10 seconds with elapsed time and the remaining request budget.
Application listing, launch and installation identify their operation after
boot, with a separate bounded execution time. A lifecycle request, including
time queued behind another request, stops waiting after 250 seconds. Service
startup is a separate step bounded at 160 seconds; authentication prompts wait
for your response.

If Android does not become ready, the command exits with `Android looks stuck`,
the last observed boot stage, and journal commands. A readiness timeout does not
stop Android. After inspecting the logs you can follow its progress again:

```console
droidloomctl wait
```

Use `droidloomctl start --no-wait` when you only want to start the services;
its success message explicitly says Android readiness has not been checked.
`--json` keeps a single JSON result on stdout and suppresses progress messages.
If a request times out while an app operation is in progress, it may still finish
in the background; check its result before retrying.

## 4. Try a built-in app

```console
droidloomctl applications
droidloomctl launch com.android.settings
```

A separate Settings window should appear. Optionally set Android's display
density; 160 DPI was used for the Omarchy preview tests:

```console
droidloomctl dpi 160
```

The density persists. Desktop tiling determines the window's available size.

## 5. Install an APK

The installation command accepts a **single, standalone APK**. Native x86_64
APKs and apps without native libraries remain preferred. Revision 15 adds
experimental ARM64 support through the source-built Digitalis/Berberis translator.
ARM32 apps are unsupported; ARM64 compatibility and performance vary by app.

Download the APK from its publisher or another source you trust. Keep it on the
Linux host; you do not need to copy it into Android first.

Start Droidloom, then install the APK as your ordinary desktop user:

```console
droidloomctl start
droidloomctl install "$HOME/Downloads/application.apk"
```

No sudo is needed for installation. The client opens the APK with your permissions
and passes its file descriptor to the authenticated runtime daemon, which feeds
it to Android's package manager. The command waits for boot readiness, updates
an existing app while retaining its data, and reports success only when Android
confirms installation. The desktop catalog refreshes automatically.

Use `--user <id>` to select an Android user (default: `0`), or `--json` for a
machine-readable response. Files must be regular APK archives no larger than
2 GiB. Both `droidloomctl` and `droidloomd` must include the install command;
older preview packages need an update.

If installation reports `device is still booting`, wait and retry. An ABI error
usually means the APK needs an unsupported CPU architecture. A `.apkm`, `.apks`
or `.xapk` bundle is not a standalone APK: renaming it will not work. Split-package
and additional game-data installation are not covered by this preview guide.

Android storage is limited by the cell's `data.img`, independently of the free
space on the Linux host. First-time setup creates a 12 GiB data disk, which can
be too small for games that download additional assets. If Android reports
insufficient storage, check `/data` and `/storage/emulated/0` inside the cell.
To enlarge an existing disk, stop Droidloom, verify its loop devices are detached,
back up the selected `data_dir` and cell configuration, run an offline writable
ext4 check, enlarge the image file, and grow its filesystem with `resize2fs`.
Check the filesystem again before restarting. Growth preserves installed apps
and accounts; package setup retains existing image sizes. A sparse image still
needs enough host space for future writes. The development desktop's disk was
expanded to 64 GiB for NTE's additional assets.

## 6. Launch the installed app

The desktop catalog updates automatically. Find the app in your desktop launcher
or list its package and launcher activity:

```console
droidloomctl applications
droidloomctl launch com.whatsapp
```

Replace `com.whatsapp` with your app's package name. If explicit activity selection
is needed, use the component shown in the catalog:

```console
droidloomctl launch com.whatsapp --component com.whatsapp/com.whatsapp.Main
droidloomctl launch com.halfbrick.fruitninjafree \
  --component com.halfbrick.fruitninjafree/com.halfbrick.mortar.MortarGameLauncherActivity
```

These examples assume you installed WhatsApp or the tested Fruit Ninja 2.8.9
x86_64 APK. APKs are not bundled with Droidloom. Complete app registration and
first-run prompts yourself. Revision 11 handles Fruit Ninja's launcher and
age-screen handoffs without requiring an activity-specific retry; earlier
revisions are affected by the [known launch issue](KNOWN_ISSUES.md).

For games that choose their rendering size only at startup, set the initial
Android task resolution in pixels:

```console
droidloomctl launch com.hottagames.nte \
  --component com.hottagames.nte/com.epicgames.unreal.SplashActivity \
  --resolution 2560x1440
```

`--resolution WIDTHxHEIGHT` restarts that application and supplies its task
bounds before Android starts the activity. Each dimension must be between 1 and
16384. Ordinary launches keep their existing behavior. This sets the Android
launch size; game-specific rendering-scale settings remain under the app's control.

To keep a resolution in a generated `.desktop` launcher, append the option to
its `Exec=` line and add `X-Droidloom-Resolution=2560x1440` in the `[Desktop Entry]`
section. The catalog validates and preserves that key during refreshes, including
app updates, and regenerates the matching command-line option.

NTE 1.3.1 also needs its Unreal mobile render scale overridden: a 2560x1440
Android window alone still produced approximately 1440x792 game buffers. On the
tested installation, its existing Unreal launch-file reader accepts
`-mcsf=0 -mobileresx=2560 -mobileresy=1440` in
`/data/user/0/com.hottagames.nte/files/UnrealGame/HT/UECommandLine.txt`, appended
to the original project/map command line. Preserve an existing launch file before
changing it and restart only NTE afterward. Do not replace its encrypted
`GameUserSettings.ini` or `Engine.ini` with plain-text Unreal settings.
The native-resolution launch produced 2536x1392 game buffers inside the desktop's
2542x1397 decorated work area; fullscreen removes that work-area constraint.
This is an NTE-specific setup, separate from the general `--resolution` option.

## Stop, restart, upgrade and remove

```console
droidloomctl stop
droidloomctl start
```

Startup is manual: app launch does not currently start a stopped runtime.
Stopping releases the runtime's resources and closes its Android windows.

To upgrade, install the new matching package pair with the same `pacman -U`
command, then run `droidloomctl start`. The transaction stops Droidloom and retains
Android data. Installing an older pair restores software, not changes to app data.

To uninstall:

```console
sudo pacman -R droidloom-runtime droidloom-image
```

Sudo lets pacman stop the system service, remove package-owned files and update
its database. Removal cleans exported desktop launchers and the generated service
permission. **Android app data and personal configuration are retained**, so a
reinstall is not a factory reset. Back them up before any deliberate data reset.

For launch failures, collect diagnostics before restarting:

```console
droidloomctl status
droidloomctl logs -n 500
droidloomctl crashes com.whatsapp
journalctl --user -u droidloom.service -n 100 --no-pager
journalctl -b -u droidloomd.service -n 200 --no-pager
```

These commands do not require sudo. Replace the example package in crash reports
with the failing app. See [KNOWN_ISSUES.md](KNOWN_ISSUES.md) for current limits.
