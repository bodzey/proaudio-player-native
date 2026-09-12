# Gain safety invariants

The player never relies on positive digital or hardware gain to reach normal listening level.

Programme receivers are transports and remain at unity. MUSIC and MASTER are user/policy gain stages limited to unity or attenuation. The selected physical PipeWire sink is fixed at unity. Hardware mixer normalization is used only when ALSA exposes a trustworthy non-positive dB mapping; a control whose reported 0 dB resolves to 0% is treated as invalid and is not accepted as unity.

MUSIC and ALERT are mixed in PipeWire's floating-point graph. ALERT enters MASTER at unity (0 dB); priority is implemented by ducking MUSIC before announcement playback, not by permanently attenuating the alert signal. The default MUSIC duck is -12 dB.

The final `MASTER -> output` graph link carries a fixed -3 dB attenuation. Graph-link gains are applied as absolute Pulse raw volumes, so repeated hotplug/reconciliation is idempotent and cannot accumulate attenuation.

With the default two-bus policy, one programme source is audible on MUSIC, MUSIC is ducked before ALERT playback, and the final -3 dB stage leaves sample-domain margin for the sum. The normal programme path therefore does not reserve a large permanent pad merely for a rare alert.

This is not a general-purpose guarantee for an arbitrary number of simultaneous full-scale streams or for user settings that disable/reduce ducking. If the product later permits unrestricted simultaneous sources while requiring a hard output ceiling, that ceiling belongs after MASTER as a graph-scheduled PipeWire limiter/processor, not as an asynchronous Pulse capture/playback relay.

The retired standalone Rust lookahead limiter remains outside the permanent path because it introduced an asynchronous userspace bridge between MASTER and the hardware clock.
