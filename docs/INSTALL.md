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
can take longer while Android initializes its data. A running service does not
necessarily mean Android's package manager has finished booting.

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

There is currently no dedicated APK installation command or drag-and-drop
installer. The following is the manual procedure used for the preview. It installs
a **single, standalone APK**. Use an x86_64 APK, or an app with no native libraries;
ARM-only APKs need translation that Droidloom does not currently provide.

Download the APK from its publisher or another source you trust. Keep it on the
Linux host; you do not need to copy it into Android first.

Find the host PID of the running Android init process:

```console
pgrep -af '^/init second_stage$'
```

The first number is the PID. If multiple Android containers are running, identify
the Droidloom entry before continuing: `cat /proc/<PID>/cgroup` should show
`droidloomd.service`. Do not use an arbitrary container PID. Set these two values,
replacing the example PID and APK path:

```console
ANDROID_INIT_PID=12345
APK="$HOME/Downloads/application.apk"
```

Confirm boot has completed:

```console
sudo nsenter --target "$ANDROID_INIT_PID" \
  --mount --uts --ipc --net --pid --cgroup --root --wd --env \
  -- /system/bin/getprop sys.boot_completed
```

Wait until it prints `1`. Then install:

```console
sudo nsenter --target "$ANDROID_INIT_PID" \
  --mount --uts --ipc --net --pid --cgroup --root --wd --env \
  -- /system/bin/cmd package install -r -S "$(stat -c %s -- "$APK")" < "$APK"
```

Sudo is needed to enter Droidloom's Android namespaces and invoke its package
manager. The host reads the APK and streams it into Android. `-r` permits updating
an existing installation while retaining its data. The expected result is
`Success`. Re-identify the PID after every Android restart.

If installation reports `device is still booting`, wait and retry. An ABI error
usually means the APK needs an unsupported CPU architecture. A `.apkm`, `.apks`
or `.xapk` bundle is not a standalone APK: renaming it will not work. Split-package
and additional game-data installation are not covered by this preview guide.

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
first-run prompts yourself. Fruit Ninja's age-screen transition may require
repeating the launch command once; see [known issues](KNOWN_ISSUES.md).

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
```

These commands do not require sudo. Replace the example package in crash reports
with the failing app. See [KNOWN_ISSUES.md](KNOWN_ISSUES.md) for current limits.
