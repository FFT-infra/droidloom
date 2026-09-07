# Droidloom threat model v1

These are security design requirements, not a claim that the preview enforces
every control.

The current preview still needs production isolation hardening. In particular,
requirements below for subordinate UID mapping, seccomp and signed distribution
must not be read as evidence of enforcement.

## Security claim

The primary boundary is designed to contain an untrusted Android application
that has not escaped Android's application sandbox. Defense in depth should
also make a framework or cell compromise harder to turn into host compromise,
but version 1 does **not** claim VM-equivalent containment of a fully
compromised Android cell. The host kernel, selected DRM render driver, host compositor,
and `droidloomd` remain trusted computing base.

## Principals and trust

| Principal | Trust | Authority |
| --- | --- | --- |
| Android APK and native libraries | Untrusted | Android app UID, granted Android permissions |
| Android framework/cell | Partially trusted | Private namespaces, Binder contexts, delegated cgroup |
| Per-user presenter and catalog | User-trusted | Lifecycle requests and narrow host portals for that user |
| `droidloomd` | Host-trusted, minimal | Namespace/mount/device/cgroup/network construction only |
| Host compositor | Host-trusted | Display, input, shell, task, capture, and power policy |
| Image/package manager | Host-trusted | Signed immutable system content and explicit activation |
| Linux kernel and selected GPU driver | Trusted computing base | Shared execution and render-node ABI |

## Protected assets

- host filesystem content outside explicit portals;
- host UID identities and credentials;
- host routes, firewall, sockets, and other users' traffic;
- physical display/KMS, input devices, camera, microphone, sensors, USB, modem,
  Bluetooth, and power controls;
- other users' Android cells and `/data`;
- The host compositor's private control channels and compositor-owned buffers;
- package-owned immutable Android system content.

## Mandatory controls

1. Map Android IDs into a dedicated, non-overlapping subordinate range. Never
   map Android root to host root or the interactive user's host UID.
2. Create private PID, mount, IPC, UTS, network, cgroup, and Binder namespaces.
3. Assemble mounts and devices in the supervisor. Android receives no general
   host mount authority and no broad `/dev` bind mount.
4. Expose only the selected DRM render node and the explicitly configured
   graphics backend's bounded auxiliary devices. `kgsl_dma_heap` adds only
   `/dev/kgsl-3d0` and `/dev/dma_heap/system`; their drivers become part of the
   trusted kernel interface. Validate device identities and use private inodes
   so cell permissions cannot alter host nodes. DRM card/KMS and physical input
   nodes are forbidden.
5. Put Android networking behind a private veth and host-owned policy. Android
   `netd` never shares host route/firewall authority.
6. Authenticate the private compositor and lifecycle sockets with filesystem
   ownership plus peer credentials. Treat task/package identity sent by the
   Android side as untrusted until reconciled with the session service.
7. Validate and bound every message, string, object count, dimension, file
   descriptor, DMA-BUF plane, format/modifier, fence, and lifecycle transition.
8. Apply no-new-privileges and seccomp after construction; retain only the
   capabilities proven necessary for Android boot.
9. Keep system/vendor/product immutable and package signed. `/data` is the only
   normal mutable Android state.
10. Make teardown idempotent and host-owned. A dead or hostile cell must not be
    needed to release its cgroup, mounts, veth, Binder instance, or buffers.

## Boundary-specific attacks

- Binder: use a private binderfs instance and three private contexts; never
  expose host Binder devices.
- Denial protocol/DMA-BUF: reject truncated sequenced records, ancillary-data
  truncation, invalid plane counts, descriptor-role mismatches, overflows,
  unsupported format/modifier pairs, foreign devices, impossible dimensions,
  stale timeline points, and ambiguous ownership. Retain buffers until GPU/KMS
  completion.
- Portals: use typed, least-authority requests; pass individual descriptors
  instead of paths or host directory mounts.
- Image activation: validate schema/ABI and artifact hashes before an atomic
  version switch. Never execute hooks from an untrusted image.
- Network: host policy owns NAT/routing. Prevent spoofing outside the assigned
  veth and tear policy down by stable cell identity.
- Resource exhaustion: cap processes, memory, CPU, file descriptors, Binder
  allocations, surfaces, buffers, and queued messages at the host cgroup and
  protocol layers.

## Residual risks and escalation

Kernel, Binder, GPU, and shared-filesystem vulnerabilities can cross the cell
boundary. Android SELinux cannot install a private policy into a conventional
shared host kernel. Protected media, hardware attestation, and Play Integrity
are not supported claims. Workloads requiring containment after total cell
compromise must use a separately designed hardened VM mode.

Any new device service, host portal, privileged capability, shared namespace,
or protected-buffer path changes this threat model and requires a new version.
