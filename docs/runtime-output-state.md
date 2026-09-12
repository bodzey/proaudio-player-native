# Runtime output state

`/run/proaudio-player/proaudio-player-bus-modules` records the current physical
sink, effective output target and the Pulse module identifiers that make up the
persistent graph. `OUTPUT_LOOP_MODULE` is the final MASTER-to-output graph link.
The target is the selected physical sink when one exists and PARKING otherwise.
Output switching replaces this module only; MUSIC, ALERT and MASTER buses stay
alive so user gain state and source streams are not rebuilt during a device switch.

The output watcher treats this file as a hint, not proof of health. Before
skipping reconciliation it verifies that all logical sinks and all three saved
loopback modules still exist. For a physical target it additionally requires the
software sink to be unmuted at 100%. A PipeWire restart, a stale state file, a
hot-unplug or an external physical-volume change therefore triggers repair even
when the selected sink name did not change.
