# Mixing headroom

The normal music path is kept at unity for maximum useful DAC resolution. Programme sources are mutually exclusive on MUSIC. Alert playback is sequenced after MUSIC ducking, so the player does not reserve a permanent -6 dB pad on all music merely to accommodate a rare second signal.

The default alert policy uses -12 dB MUSIC ducking. A future graph-native emergency ceiling may be added as a PipeWire filter, but it must remain sample-transparent below the ceiling and must not reintroduce an asynchronous PCM bridge.
