# Cell audio output

Android keeps ownership of audio policy, mixing, routing and volume. The cell's
audio HAL renders the primary output mix at the format that policy already
declares and writes those samples to the endpoint the supervisor exposes inside
the cell as `/dev/socket/droidloom/audio`. The host `droidloom-audio` user
service owns the other end and hands the stream to the session's PipeWire
server. The cell needs no host device node, no host audio library and no
privileged helper, and the bridge needs no Android image change.

## Transport

Raw signed 16-bit little-endian PCM, 48 000 Hz, stereo, no header, no framing,
no timestamps, one stream per connection. The Android primary output already
declares `PCM_16_BIT`/48000/STEREO, so neither side resamples or remixes; the
socket is a byte pipe and the writer's pacing is the clock.

The endpoint is `<XDG_RUNTIME_DIR>/droidloom/audio.sock`, mode 0666, created by
`droidloom-audio` with the same private-directory and replacement checks the
presenter applies to its own endpoints. The supervisor mounts it into the cell's
private `/dev` only when it exists, so a host without the audio service still
boots. `droidloom.service` orders itself after `droidloom-audio.service`, so the
endpoint exists before any cell starts.

`droidloom-audio` serves one stream at a time; a second connection waits for the
current stream to end. Each stream is piped into `pw-cat --playback --raw`, one
child per stream. The pipe is bounded, so a stalled sink stops the reader, which
stops reading the cell socket, and the cell is back-pressured instead of the
host buffering latency. A sink failure ends that stream only: graphics, input
and the catalog are unaffected, and the next connection is served normally.

Restarting `droidloom-audio` alone does not reach a running cell: the mount pins
the socket inode, so a rebound endpoint leaves the cell writing to the closed
one until the next cell setup. Like every other cell endpoint, the bridge takes
effect when the runtime restarts, which is what `Restart=on-failure` and the
`PartOf` propagation both produce.

## Volume, capture and limits

Android's volume keys scale the mix before it leaves the cell, so they keep
working; the host sink keeps its own level and mute state, and the two are not
yet synchronized. Microphone capture is not part of this version, and neither is
per-stream control (multiple simultaneous Android outputs are mixed inside the
cell by the audio policy, not by the host).

A cell that fills the pipe faster than the sink drains it blocks in `write`,
which stalls Android's audio thread. Dropped-and-recovered playback is therefore
the sound server's responsibility, not the bridge's.
