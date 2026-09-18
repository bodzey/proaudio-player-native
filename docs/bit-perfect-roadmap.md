# Direct / bit-perfect mode roadmap

Normal appliance mode uses an adaptive mixed processing domain: MUSIC, ALERT and MASTER stay available while PipeWire may follow a compatible active-stream rate. The graph remains float32 and may resample when a source/device combination cannot share one rate.

A future direct mode should be opt-in and mutually exclusive with mixed playback. Its contract should be:

- exactly one programme source;
- no alert or notification mixing while direct mode is active;
- no MUSIC or MASTER attenuation unless the user explicitly leaves direct mode;
- no resampling when the physical device supports the source rate;
- physical PipeWire and ALSA gain stages at verified unity;
- source format/rate negotiated directly to the selected DAC;
- automatic return to normal mixed mode before an alert or other system audio must play.

This keeps the normal network-player mode robust while allowing a true bit-perfect path where the hardware supports it.
