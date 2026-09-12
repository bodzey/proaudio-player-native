# Runtime output state

`/run/proaudio-player/proaudio-player-bus-modules` records the currently routed physical sink and the Pulse module identifiers that make up the persistent graph. `OUTPUT_LOOP_MODULE` is the final MASTER-to-hardware graph link. Output switching replaces this module only; MUSIC, ALERT and MASTER buses stay alive so user gain state and source streams are not rebuilt during a device switch.

The output watcher treats this file as a hint, not proof of health. Before skipping reconciliation it verifies that all logical sinks and all three saved loopback modules still exist, and that the selected physical software sink is unmuted at 100%. A PipeWire restart, a stale state file or an external physical-volume change therefore triggers repair even when the selected sink name did not change.
