# Gain safety invariants

The player must never rely on positive digital or hardware gain to reach normal listening level.

For programme playback, source streams are kept at unity, MUSIC and MASTER are limited to 0 dB or attenuation, the physical PipeWire sink is fixed at unity, and any hardware mixer normalization must resolve to a verified value at or below 0 dB.

Alerts are mixed only after MUSIC ducking has completed. The current default duck is -12 dB. This provides mix headroom for announcement playback while keeping the normal music path at unity when no alert is active.

The previous standalone Rust lookahead limiter was removed from the permanent signal path because it introduced an asynchronous Pulse capture/playback relay between MASTER and the hardware clock. The permanent output path now remains inside the PipeWire graph. If a final emergency ceiling is added later, it must be implemented as a graph-scheduled PipeWire filter rather than a polling PCM bridge.
