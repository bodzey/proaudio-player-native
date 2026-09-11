# Hardware gain policy

Physical PipeWire sinks are fixed at software unity. WirePlumber is configured with `api.alsa.soft-mixer = true`, so user-facing sink volume does not silently manipulate arbitrary ALSA hardware controls.

When `HARDWARE_MIXER_MODE=unity`, the routing helper inspects playback-only ALSA controls with a reported dB scale. It attempts 0 dB and, if the control reports positive gain, backs off below 0 dB. Capture/input/boost/gain/tone controls and controls without a usable dB scale are not guessed. If a safe value cannot be verified, the original raw value is restored.

Device-specific quirks belong in firmware hardware profiles rather than in the generic player core.
