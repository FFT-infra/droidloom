# First-pixels APEX classpath projection

The pinned Android 17 image contains `com.android.virt` as an EROFS-payload
APEX. A host kernel without EROFS support cannot activate that one module, but
`framework.jar` still references its `VirtualizationFrameworkInitializer`
during Zygote preload.

For development boot only, Droidloom extracts and verifies the module's
`etc/classpaths/bootclasspath.pb` and `javalib/framework-virtualization.jar`,
projects them read-only below `/droidloom`, and runs the Rust
`droidloom-classpath-wrapper` in place of the ordinary `derive_classpath`
one-shot. The wrapper bind-mounts the projection at `/apex/com.android.virt`
and can apply explicitly listed, regular-file overrides from the same
read-only `/droidloom` projection to existing paths below `/apex`. The
first-pixels configuration uses that bounded mechanism for the patched
`netbpfload` binary that tolerates unrelated BPF IDs in the shared host kernel
while still loading Android's real maps and programs. It also projects a
`libservice-connectivity.so` that skips only SELinux-label verification when
the backing filesystem reports that security xattrs are unsupported or the
host-created bpffs object has no SELinux label inside a Droidloom cell;
ownership, modes, and real BPF object access are still checked.
The module utility JNI library also tolerates an unavailable `AF_KEY` socket
during development-cell bootstrap. This is a target-kernel compatibility
exception, not a fake network implementation: Android's real `netd`, DNS
resolver, BPF programs, IpClient, and ConnectivityService remain active. The
cell reaches the host connection through its private veth and host-owned
nftables policy. The exception must be removed or replaced with an equivalent
RCU primitive before production permits runtime BPF map rotation.
The platform `libmeminfo` projection reports absent in-cell GPU BPF accounting
as zero only for a marked Droidloom cell. If the map exists, Android's strict
key/value/permission validation remains unchanged.
The projection also supplies `libnetd_updatable.so` with the maintained
`0009-netd-bpf-pid-namespace.patch`. Netd's identity map must contain the TGID
used by `bpf_get_current_pid_tgid`, not a container-local `getpid()` value.
Inside a marked Droidloom cell, a temporary, unattached BPF syscall program
queries that identity using the existing CAP_BPF capability. It is run once
and closed, and failures remain fatal. Android's root/TGID authorization
checks and networking self-tests are unchanged. This path was verified on
Motorola Linux 6.6.98, including the real `netd1shot` startup self-test.

The wrapper then delegates all classpath generation to Android's own binary. It
does not start virtualization services or emulate an APEX manager.

The same development `init.rc` delta preserves the private binderfs instance
that the namespace supervisor mounts and populates before Android second
stage. Stock init must not overmount that instance with an empty binderfs;
doing so leaves its `/dev/{binder,hwbinder,vndbinder}` symlinks dangling.

`0001-droidloom-classpath-projection.patch` records the exact development
`init.rc` delta. Production images should use a kernel-supported APEX payload
format and remove this compatibility path.
