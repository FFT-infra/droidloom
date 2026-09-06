# Binder compatibility for development cells

Droidloom uses a private Binder filesystem inside each Android cell. The Aston
host kernel currently has no active Android SELinux LSM, so it cannot translate
Binder transaction security IDs into Android security-context strings.

The tracked patches keep Android's Binder ABI and service implementations
intact while adapting that missing kernel facility only when `DROIDLOOM_CELL`
is present:

- `0001` prevents the context manager from requesting unsupported transaction
  security contexts.
- `0002` lets servicemanager use its existing PID-context fallback.
- `0003` prevents ordinary service nodes from requesting unsupported transaction
  contexts and supplies the synthetic development-cell context used by the
  libselinux compatibility layer.

Outside a marked Droidloom cell, all upstream SID behavior remains unchanged.
The development cell relies on the host sandbox as its security boundary;
production confinement must not claim Android SELinux isolation until the
kernel facility and policy are present.
