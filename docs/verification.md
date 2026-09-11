# Audio verification

After changing the graph, verify the appliance in this order:

1. `proaudio-player-buses.service`, PipeWire, pipewire-pulse, WirePlumber and the native daemon are active.
2. `pactl list short sinks` shows MUSIC, ALERT, MASTER and the selected physical sink.
3. `pactl list short sink-inputs` shows two internal bus loopbacks into MASTER and one final MASTER loopback into the selected physical sink while audio is active.
4. The selected physical sink becomes `RUNNING` during playback; an unselected sink remains idle or suspended.
5. MUSIC and MASTER meters move during programme playback.
6. Switching between built-in and USB output moves only the final MASTER loopback; MUSIC/ALERT/MASTER modules keep their identities and current user volumes.
7. The physical PipeWire sink remains 100% and the ALSA playback control, when normalized, never reports positive dB gain.
8. Spotify, AirPlay, MPD and DLNA each arrive at MUSIC at unity; source arbitration leaves only one programme source audible.
9. Alert playback ducks MUSIC before ALERT becomes audible, then restores the saved MUSIC state.
10. Reboot and hot-unplug/replug preserve the selected output and user MUSIC/MASTER state.
