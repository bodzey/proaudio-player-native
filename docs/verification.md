# Audio verification

After changing the graph, verify the appliance in this order:

1. `proaudio-player-buses.service`, PipeWire, pipewire-pulse, WirePlumber and the native daemon are active.
2. `pactl list short sinks` shows MUSIC, ALERT, MASTER and the selected physical sink.
3. `pactl list short sink-inputs` shows two internal bus loopbacks into MASTER and one final MASTER loopback into the selected physical sink while audio is active; all three links report 0 dB/unity.
4. The selected physical sink becomes `RUNNING` during playback; an unselected sink remains idle or suspended.
5. MUSIC and MASTER meters move during programme playback.
6. Switching between built-in and USB output moves only the final MASTER loopback; MUSIC/ALERT/MASTER modules keep their identities and current user volumes.
7. The physical PipeWire sink remains 100% and the ALSA playback control, when normalized, never reports positive dB gain.
8. Spotify, AirPlay, MPD and DLNA each arrive at MUSIC at unity; source arbitration leaves exactly one programme sink-input audible, including after a receiver reconnect.
9. Alert playback finishes MUSIC ducking before ALERT becomes audible, caps ALERT to the remaining linear peak budget, then restores both the saved ALERT fader and MUSIC state.
10. With MUSIC=MASTER=100% and no alert, a 0 dBFS test signal has identical sample amplitude at the source, MUSIC monitor, MASTER monitor and physical software sink monitor.
11. With a full-scale music test signal ducked by -12 dB and a simultaneous full-scale alert, the physical software sink peak does not exceed 0 dBFS.
12. Reboot and hot-unplug/replug preserve the selected output and user MUSIC/ALERT/MASTER state.
