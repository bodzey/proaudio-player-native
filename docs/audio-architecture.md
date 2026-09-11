# Audio architecture

ProAudio Player treats PipeWire as the realtime audio engine and the Rust daemon as the control plane.

The normal playback path is:

```text
Spotify / AirPlay / MPD / DLNA
            |
            v
     proaudio_player_music
            |
            +------------------+
                               |
alerts -> proaudio_player_alert|
                               v
                    proaudio_player_master
                               |
                               v
                    selected physical sink
                               |
                               v
                              ALSA
                               |
                               v
                              DAC
```

The MUSIC, ALERT and MASTER buses and the final MASTER-to-device connection are PipeWire/Pulse graph links. There is no polling userspace PCM relay in the permanent output path.

## Gain policy

- Source receivers are transports and stay at unity. They do not own user volume.
- MUSIC owns normal programme level and alert ducking.
- MASTER is the user-facing final digital gain stage and is limited to unity or attenuation; positive digital gain is not allowed.
- The selected physical PipeWire sink is fixed at 100%/unity and is not used as a user gain control.
- WirePlumber uses a software mixer for physical ALSA devices so desktop-style sink volume cannot silently move an arbitrary hardware mixer.
- The routing helper may normalize a trustworthy ALSA playback control to 0 dB or the nearest verified value below 0 dB. It never intentionally selects positive hardware gain. Unknown controls are not guessed.
- Source arbitration keeps only one programme source audible on MUSIC at a time.
- Alert playback is preceded by MUSIC ducking; alert and minute-silence media are separate from the programme path.

This keeps normal music playback linear. Protection against overload is primarily gain staging and source exclusivity, not continuous dynamics processing.

## Sample-rate policy

`/etc/proaudio-player-alert/audio.env` is the processing-rate authority. The current appliance uses a fixed 48 kHz processing domain and lets PipeWire perform boundary conversion for sources or hardware that use another rate. This avoids rebuilding the live graph when a transport changes format.

A future direct/bit-perfect mode can bypass mixing and user DSP for a single source, switch the hardware clock to the source rate, and disable alerts for the duration of direct playback. That mode is intentionally separate from the normal mixed appliance mode.
