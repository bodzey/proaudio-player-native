# Hardware gain policy

Physical PipeWire sinks are fixed at software unity. WirePlumber is configured with `api.alsa.soft-mixer = true`, so user-facing sink volume does not silently manipulate arbitrary ALSA hardware controls.

The generic player defaults to `HARDWARE_MIXER_MODE=off`: it does not write ALSA hardware mixer controls during runtime routing. This keeps the core hardware-neutral and avoids analogue transients caused by changing arbitrary mixer controls while an amplifier is connected.

`HARDWARE_MIXER_MODE=unity` remains available only as an explicit, validated firmware/hardware-profile opt-in. In that mode the routing helper inspects playback-only ALSA controls with a reported dB scale, targets a safe value at or below 0 dB, and restores the original raw level when the mapping is not trustworthy.

Physical ALSA playback nodes use `session.suspend-timeout-seconds = 0`, so switching the final PipeWire route does not repeatedly close and reopen the DAC. Device-specific mixer or power-sequencing quirks belong in firmware hardware profiles rather than in the generic player core.
