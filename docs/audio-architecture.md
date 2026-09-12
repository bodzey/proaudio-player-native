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
                         unity (0 dB)
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

If no physical output exists, the final link terminates at `proaudio_player_parking`,
a silent null sink. The logical buses and control API remain alive. When a DAC
appears, only the final link is replaced; receiver streams, bus identities and
user mixer state are preserved. If the DAC disappears, the same link returns to
parking instead of tearing down the audio graph.

## Gain policy

- Source receivers are transports and stay at unity. They do not own user volume.
- Source arbitration keeps only one programme source audible on MUSIC at a time.
- MUSIC owns normal programme level and alert ducking.
- ALERT enters MASTER at unity; its own fader controls announcement level, while priority is created by ducking MUSIC before playback.
- MASTER is the user-facing final digital gain stage and is limited to unity or attenuation; positive digital gain is not allowed.
- MUSIC-to-MASTER, ALERT-to-MASTER and MASTER-to-physical graph links are fixed at unity.
- Fixed graph gains are applied as absolute Pulse raw volumes, so repeated routing reconciliation is idempotent.
- The selected physical PipeWire sink is fixed at 100%/unity and is not used as a user gain control.
- WirePlumber uses a software mixer for physical ALSA devices so desktop-style sink volume cannot silently move an arbitrary hardware mixer.
- Hardware mixer normalization is accepted only when the dB mapping is trustworthy. In particular, a reported 0 dB value that resolves to 0% is rejected rather than treated as unity.
- Graph construction is transactional: a failed module load or gain setup unloads every module created by that attempt instead of leaving a partial parallel route.

This keeps the permanent path linear and inside PipeWire. The source arbiter permits exactly one programme sink-input on MUSIC, including during receiver reconnects. Before an announcement starts, the MUSIC ducking transition completes; the announcement is then capped only to the remaining linear sample-peak budget. The two maximum possible bus contributions therefore add to no more than unity. The ALERT fader is restored after every announcement, including failed playback.

Talkover duck/play/restore is serialized with MUSIC and ALERT fader updates.
A concurrent API adjustment therefore cannot invalidate the peak budget halfway
through an announcement or be lost when the pre-announcement state is restored.

This complementary linear mix law protects the current two-bus appliance without a permanent headroom penalty, nonlinear processing or an asynchronous userspace bridge. It assumes each admitted source itself remains within full scale. A future mode that permits arbitrary simultaneous streams would require a graph-native post-mix limiter and is outside this contract.

## Sample-rate policy

`/etc/proaudio-player-alert/audio.env` is the processing-rate authority. The current appliance uses a fixed 48 kHz processing domain and lets PipeWire perform boundary conversion for sources or hardware that use another rate. This avoids rebuilding the live graph when a transport changes format.

A future direct/bit-perfect mode can bypass mixing and user DSP for a single source, switch the hardware clock to the source rate, and disable alerts for the duration of direct playback. That mode is intentionally separate from the normal mixed appliance mode.

## Failure domains

PipeWire, the logical bus service, the output watcher and the Rust control plane
are independently supervised. The daemon therefore keeps the API and persisted
state available while PipeWire or the buses restart. It observes logical sink
identities and reapplies the saved MUSIC/ALERT/MASTER state once per newly created
topology; it does not repeatedly overwrite live user changes.
