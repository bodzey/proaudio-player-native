# Mixing headroom

The normal programme path keeps receiver streams at unity and uses MUSIC/MASTER as the only user-facing digital gain stages. Source arbitration keeps one programme source audible on MUSIC at a time.

Alerts are a second, priority bus. ALERT enters MASTER through a unity graph link. Before an announcement starts, MUSIC is ducked; the default is -12 dB.

The final MASTER-to-physical link is also unity. Normal playback is therefore not reduced to reserve headroom for an event that is not occurring.

For announcement playback, the daemon reads the effective, already-ducked MUSIC gain and the user-selected ALERT fader per channel. ALERT is never rewritten by playback policy. If the coherent worst-case sum would exceed unity, only MUSIC yields temporarily:

```text
music = PulsePercentToLinear(current MUSIC)
alert = PulsePercentToLinear(user ALERT) * event gain
safe_music = min(music, 1 - alert)
```

The resulting MUSIC value is converted back to Pulse's cubic percentage scale and restored to the normal ducked value after the file ends or fails. A per-event level, such as the minute-silence file level, is applied inside the alert player as attenuation relative to the ALERT fader; it never replaces or moves that fader. Thus the user-selected alert level is authoritative, both sources remain audible whenever its headroom permits, and their worst-case coherent sample sum cannot exceed 0 dBFS. Announcement calls are serialized so two alert streams cannot overlap and invalidate the budget.

All permanent graph gains are written as absolute Pulse raw unity volumes. Re-running output reconciliation therefore restores 0 dB rather than accumulating a relative change.
