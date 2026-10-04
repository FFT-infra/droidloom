# Audio integration proposal

See [Host service integration](architecture.md#host-service-integration-direction) for the shared SMS/call/audio ownership model, Linux-generic backend boundaries and implementation milestones.

Status: proposed; the current audio HAL provides silent stub streams.

Implement an Android AIDL audio HAL adapter backed by the host PipeWire service.
Keep Android audio policy and app APIs intact; let the host own physical devices.
Transport PCM through bounded shared-memory buffers, separating stream control
from sample transfer and avoiding extra queues or copies where practical.

Start with playback and microphone capture, including routing, mute, disconnect
recovery and access policy. Validate latency, underruns, CPU cost and concurrent
Linux/Android playback before tuning buffer sizes. Direct ALSA ownership remains
an alternative for dedicated Android hardware.
