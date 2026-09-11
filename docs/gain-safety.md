# Gain safety invariants

The player must never rely on positive digital or hardware gain to reach normal listening level.

For programme playback, source streams are kept at unity, MUSIC and MASTER are limited to 0 dB or attenuation, the physical PipeWire sink is fixed at unity, and any hardware mixer normalization must resolve to a verified value at or below 0 dB.

The final `MASTER -> output` graph link carries a fixed -3 dB safety headroom. The `ALERT -> MASTER` graph link carries an additional -9 dB attenuation. Even if MUSIC were accidentally left unducked, two simultaneous full-scale sample peaks remain below 0 dBFS after the final headroom stage. The normal alert policy still ducks MUSIC by -12 dB before announcement playback, providing substantially more practical mix margin.

The -3 dB final headroom is linear attenuation, not dynamics processing: it does not compress, limit, clip or otherwise reshape the waveform. It also provides useful margin for common intersample peaks before the integer/DAC boundary.

The previous standalone Rust lookahead limiter was removed from the permanent signal path because it introduced an asynchronous Pulse capture/playback relay between MASTER and the hardware clock. The permanent output path now remains inside the PipeWire graph. If a final emergency ceiling is added later, it must be implemented as a graph-scheduled PipeWire filter rather than a polling PCM bridge.
