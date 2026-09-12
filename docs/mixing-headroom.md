# Mixing headroom

The normal programme path keeps receiver streams at unity and uses MUSIC/MASTER as the only user-facing digital gain stages. Source arbitration keeps one programme source audible on MUSIC at a time.

Alerts are a second, priority bus. ALERT enters MASTER at unity (0 dB). Before an announcement starts, MUSIC is ducked; the default is -12 dB. This gives the announcement full nominal level without permanently throwing away 9 dB on every alert.

The final MASTER-to-physical link applies a fixed -3 dB attenuation. This is a linear post-mix safety margin, not a compressor or limiter. It also leaves useful room before the integer/DAC boundary.

All fixed graph gains are written as absolute Pulse raw volumes. Re-running output reconciliation therefore restores the intended gain rather than subtracting another 3 or 9 dB.

PipeWire mixes internally in floating point, so summing streams is not the same as enforcing a hard output ceiling. The current appliance avoids arbitrary programme summing and sequences MUSIC ducking before ALERT playback. If unrestricted simultaneous sources are added later, a strict ceiling should be implemented after MASTER with graph-native PipeWire dynamics processing.
