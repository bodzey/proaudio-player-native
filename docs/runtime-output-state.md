# Runtime output state

`/run/proaudio-player/proaudio-player-bus-modules` records the currently routed physical sink and the Pulse module identifiers that make up the persistent graph. `OUTPUT_LOOP_MODULE` is the final MASTER-to-hardware graph link. Output switching replaces this module only; MUSIC, ALERT and MASTER buses stay alive so user gain state and source streams are not rebuilt during a device switch.
