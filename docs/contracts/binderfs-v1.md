# Private Binder creation contract v1

`droidloomd` mounts a new binderfs instance inside the cell mount namespace at
`/dev/binderfs`. It does not bind-mount `/dev/binder`, `/dev/hwbinder`, or
`/dev/vndbinder` from the host.

While retaining only the construction authority required for this step, the
supervisor opens the instance's `binder-control` and issues
`BINDER_CTL_ADD` for exactly these names:

1. `binder`
2. `hwbinder`
3. `vndbinder`

It verifies the returned major/minor pairs belong to that binderfs instance,
sets the contract-defined ownership/modes, and creates cell-local compatibility
links `/dev/binder`, `/dev/hwbinder`, and `/dev/vndbinder`. Device identity is
recorded in teardown evidence. Failure at any point unmounts the incomplete
instance.

The host supervisor never passes a host Binder descriptor across exec. Binder
context-manager registration remains Android-owned within each private device.
Binderfs statistics are not treated as private unless the kernel actually
provides per-instance semantics. Limits on processes, memory, descriptors, and
Binder allocations are enforced outside Android as well.

Integration tests must boot two disposable cells sequentially,
prove their device identities differ from host Binder devices, and prove no
device or mount remains after each teardown. Concurrent multi-user cells are a
separate test even though the contract anticipates them.
