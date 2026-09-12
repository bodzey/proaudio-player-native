# Mixing headroom

The normal programme path keeps receiver streams at unity and uses MUSIC/MASTER as the only user-facing digital gain stages. Source arbitration keeps one programme source audible on MUSIC at a time.

Alerts are a second, priority bus. ALERT enters MASTER through a unity graph link. Before an announcement starts, MUSIC is ducked; the default is -12 dB.

The final MASTER-to-physical link is also unity. Normal playback is therefore not reduced to reserve headroom for an event that is not occurring.

For announcement playback, the daemon reads the effective, already-ducked MUSIC gain and the requested ALERT fader per channel. It temporarily applies:

```text
music = PulsePercentToLinear(current MUSIC)
alert = min(PulsePercentToLinear(requested ALERT), 1 - music)
```

The resulting ALERT volume is converted back to Pulse's cubic percentage scale and restored after the file ends or fails. Thus both sources remain audible whenever the budget permits, the alert is never amplified, and their worst-case coherent sample sum cannot exceed 0 dBFS. Announcement calls are serialized so two alert streams cannot overlap and invalidate the budget.

All permanent graph gains are written as absolute Pulse raw unity volumes. Re-running output reconciliation therefore restores 0 dB rather than accumulating a relative change.
