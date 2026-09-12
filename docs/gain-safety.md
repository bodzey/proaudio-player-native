# Gain safety invariants

The player never relies on positive digital or hardware gain to reach normal listening level.

Programme receivers are transports and remain at unity. MUSIC and MASTER are user/policy gain stages limited to unity or attenuation. The selected physical PipeWire sink is fixed at unity. Hardware mixer normalization is used only when ALSA exposes a trustworthy non-positive dB mapping; a control whose reported 0 dB resolves to 0% is treated as invalid and is not accepted as unity.

MUSIC and ALERT are mixed in PipeWire's floating-point graph. ALERT enters MASTER at unity (0 dB); priority is implemented by ducking MUSIC before announcement playback, not by permanently attenuating the alert signal. The default MUSIC duck is -12 dB.

The final `MASTER -> output` graph link carries a fixed -1 dB attenuation. Graph-link gains are applied as absolute Pulse raw volumes, so repeated hotplug/reconciliation is idempotent and cannot accumulate attenuation.

With the default policy, one programme source is audible on MUSIC and MUSIC is ducked before ALERT playback. The -1 dB final stage is intentionally only a small nominal post-mix margin so normal playback is not unnecessarily attenuated.

The -1 dB stage is not a hard ceiling for worst-case coherent simultaneous full-scale MUSIC and ALERT samples, nor for an arbitrary number of simultaneous streams. If the product later requires a mathematically strict output ceiling while allowing such sums, that ceiling belongs after MASTER as a graph-scheduled PipeWire limiter/processor, not as an asynchronous Pulse capture/playback relay.

The retired standalone Rust lookahead limiter remains outside the permanent path because it introduced an asynchronous userspace bridge between MASTER and the hardware clock.
