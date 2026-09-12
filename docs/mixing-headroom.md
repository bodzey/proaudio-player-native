# Mixing headroom

The normal programme path keeps receiver streams at unity and uses MUSIC/MASTER as the only user-facing digital gain stages. Source arbitration keeps one programme source audible on MUSIC at a time.

Alerts are a second, priority bus. ALERT enters MASTER at unity (0 dB). Before an announcement starts, MUSIC is ducked; the default is -12 dB. This gives the announcement full nominal level without permanently attenuating the alert path.

The final MASTER-to-physical link applies a fixed -1 dB attenuation. This is a light linear post-mix margin, not a compressor, limiter or hard output ceiling. It keeps normal playback close to unity while retaining a small amount of downstream margin.

All fixed graph gains are written as absolute Pulse raw volumes. Re-running output reconciliation therefore restores the intended gain rather than subtracting attenuation repeatedly.

PipeWire mixes internally in floating point, so summing streams is not the same as enforcing a hard output ceiling. The current appliance avoids arbitrary programme summing and sequences MUSIC ducking before ALERT playback. The -1 dB margin by itself does not guarantee that worst-case coherent full-scale MUSIC + ALERT samples stay below 0 dBFS. If a strict ceiling becomes a product requirement, it should be implemented after MASTER with graph-native PipeWire dynamics processing.
