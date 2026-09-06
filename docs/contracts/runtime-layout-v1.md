# Runtime and namespace layout v1

This is the construction contract for the Rust `droidloomd`. The supervisor
implements the fail-closed transaction, emits the complete typed Linux
construction/reverse-teardown plan, and provides the executable Android
cell path. `droidloomd` retains the exact namespace child handle while
`droidloomctl` supplies idempotent lifecycle operations. Production user-ID
mapping, cgroup limits, seccomp, and complete packaging hardening remain later
work; the executable path does not claim those unfinished isolation layers.

## Intended production identity

The mapping below is a design requirement, not an implemented preview guarantee.

One cell exists for one logged-in host UID. Its stable ID is `u<host-uid>`.
Android UID/GID `0..99999` maps one-to-one onto a dedicated contiguous
subordinate host range of at least 100,000 IDs. Android root is therefore an
unprivileged subordinate host ID, never host root or the interactive host UID.
Ranges must not overlap another allocation or cell.

## Namespaces

The cell receives new user, PID, mount, IPC, UTS, network, and cgroup
namespaces. Android `init` becomes PID 1 only inside the PID namespace. The
supervisor keeps a pidfd and host-side cgroup handle; process names and PID
scans are never lifecycle authority.

## Mount tree

| Cell path | Source/ownership | Required flags |
| --- | --- | --- |
| `/system` | activated package artifact | read-only, nodev |
| `/system/bin/init` | optional package-owned compatibility binary over the activated base | read-only, nosuid, nodev, executable |
| immutable partition file override | optional package-owned compatibility file at its original Android path | read-only, nosuid, nodev, executable mappings allowed |
| `/vendor` | activated package artifact | read-only, nodev |
| `/product` | activated package artifact | read-only, nodev |
| `/data` | per-user `data.img` raw ext4 filesystem | read-write, nodev, nosuid |
| `/metadata` | per-user `metadata.img` raw ext4 filesystem | read-write, nodev, nosuid |
| `/data/media/0/<collection>` | explicitly configured host-user directory | read-write idmapped bind, nodev, nosuid, noexec |
| `/proc` | private procfs | nosuid, nodev, noexec, hide host processes |
| `/sys` | minimal read-only projection/synthetic tree | read-only, nosuid, nodev, noexec |
| `/dev` | private tmpfs | nosuid, strict modes |
| `/dev/pts` | private devpts | nosuid, noexec, newinstance |
| `/dev/binderfs` | private binderfs | three named devices only |

The supervisor builds the complete tree before exec. Android receives neither
`CAP_SYS_ADMIN` nor a general view of host mounts to finish construction.
Android-controlled system state remains confined to loop-backed filesystems.
Only explicit shared-storage collections may be writable host-directory bind
mounts. They map the host UID to MediaProvider's recorded app UID and the host
GID to Android `media_rw`, and are mounted
`nodev,nosuid,noexec`, and placed behind Android's ordinary emulated-storage
service rather than exposed as Android system state.

The host kernel must provide FUSE. The packaged `vold` recursively binds its
MediaProvider lower view so configured collection submounts remain visible;
both volume teardown paths detach the whole lower tree. A non-recursive bind
exposes the underlying empty mountpoint instead of the host collection.
The supervisor reads MediaProvider's UID once from the initialized data image's
`/data/system/packages.list`; it never guesses an app UID. The test collection
uses setgid directory inheritance so Android-created files retain the mapped
host group. MediaProvider's primary GID is also mapped to a reserved subordinate
host GID, satisfying Linux's creator-credential checks before setgid inheritance.
The temporary mapping helper exits before Android init starts;
the mounts retain the kernel's mapping reference. Initial Android provisioning
must have installed MediaProvider before shared collections can be enabled.

## Device allowlist

The initial device set is null, zero, full, random, urandom, tty/ptmx/devpts,
the standard FUSE node used by Android's private emulated-storage mount,
ashmem only if an AOSP compatibility audit still requires it, the three private
Binder devices, and exactly one configured DRM render node. No `card*`, KMS,
physical input, framebuffer, camera, media, USB, Bluetooth, modem, block, raw
memory, or host control node is allowed. The synthetic sysfs exposes the
otherwise-empty fusectl mount point but no host FUSE connections.

## Host paths

- package-owned: `/usr/lib/droidloom`, `/usr/bin/droidloom-*`, and
  `/usr/share/droidloom`;
- ephemeral: `/run/droidloom/cells/u<uid>` and the user's mode-0700 runtime
  directory socket endpoint;
- system state: `/var/lib/droidloom/images` and activation metadata;
- per-user Android data: a platform-owned directory containing `data.img` and
  `metadata.img`, retained on normal package removal and deleted only by
  explicit wipe; the containing host filesystem is never exposed to Android.
- per-user shared storage: an explicit allowlist of host-owned directories,
  each mapped to one top-level collection below `/storage/emulated/0`; an
  omitted list exposes no host directory.
- per-user application integration: managed `.desktop` files below
  `$XDG_DATA_HOME/applications` with `droidloom-` names, rendered icons below the user's
  hicolor icon tree, and reconciliation state below `$XDG_STATE_HOME/droidloom`.
  Only the unprivileged user service writes these paths.
- per-user window policy: user-selected initial geometry at
  `$XDG_CONFIG_HOME/droidloom/window-policy-v1.json` and automatically
  remembered stable sizes at
  `$XDG_STATE_HOME/droidloom/window-sizes-v1.json`. Only the unprivileged
  presenter and CLI read or write these bounded, versioned documents.

No source-checkout path may appear in a runtime mount or unit.
When selected, `android_init` must be a normalized absolute path to a regular,
executable package-owned file. The supervisor bind-mounts it read-only over the
immutable base's `/system/bin/init`; omitting it retains the base init.
Each `android_file_overrides` entry names a normalized absolute package-owned
regular file and a unique normalized target below `/system`, `/system_ext`,
`/product`, or `/vendor`. Targets must already be regular files in the mounted
partition and are bind-mounted read-only with executable mappings allowed.
`/system/bin/init` remains reserved for the separately validated
`android_init` field.
Each `android_runtime_directories` entry names a normalized, package-owned host
directory and a unique, non-overlapping target below the cell-private
`/droidloom` tree. The supervisor creates the target before entering Android
and bind-mounts the directory read-only with device and setuid semantics
disabled. These projections may contain executable compatibility helpers, but
they are not Android-writable storage and cannot target `/system`, `/vendor`,
`/data`, or another Android namespace.

## Network

Each cell owns a network namespace and one veth peer. Host policy assigns its
address, forwarding, DNS broker, anti-spoofing, and NAT/firewall objects under
a stable cell tag. Android `netd` may operate only inside the cell namespace.
Teardown removes every tagged host object even if Android is unresponsive.

## Construction transaction

Construction is fail-closed and rolls back in reverse ownership order:
reserve identity/cgroup, create namespaces, populate UID/GID maps, assemble
mounts and devices, create Binder contexts, create network policy, apply
resource limits, start init, then publish the per-user lifecycle handle. A
handle is published only after every prerequisite succeeds.

Teardown first refuses new launches, terminates the pidfd-owned namespace,
waits with a bound, kills the remaining cgroup if necessary, removes network
policy/veth, unmounts the private tree, removes runtime paths, and finally
releases the cgroup. Repeating teardown must be safe.
