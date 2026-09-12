# Gain safety invariants

The player never relies on positive digital or hardware gain to reach normal listening level.

Programme receivers are transports and remain at unity. MUSIC and MASTER are user/policy gain stages limited to unity or attenuation. The selected physical PipeWire sink is fixed at unity. Hardware mixer normalization is used only when ALSA exposes a trustworthy non-positive dB mapping; a control whose reported 0 dB resolves to 0% is treated as invalid and is not accepted as unity.

MUSIC and ALERT are mixed in PipeWire's floating-point graph. Their graph links remain at unity and priority is implemented by ducking MUSIC before announcement playback. The default MUSIC duck is -12 dB.

During the short interval in which an announcement is actually audible, ALERT is capped to the linear sample-peak budget left by the already-ducked MUSIC bus. Per channel, `alert <= 1 - music`; therefore the worst-case coherent sum is no greater than unity. The persisted ALERT fader is never raised and is restored after playback.

At the defaults, full-scale MUSIC ducked by -12 dB contributes about 0.251 linear amplitude, leaving about 0.749 for ALERT (approximately -2.51 dB). If MUSIC is quieter, muted or at zero, ALERT automatically receives more of the budget up to true unity. This attenuation exists only while both buses may contribute.

All three permanent graph links carry absolute Pulse unity volume. Repeated startup, reconciliation and output switching are idempotent and cannot accumulate attenuation. Consequently, a single programme source at MUSIC=MASTER=100% reaches the physical software sink with gain exactly 1.0.

The retired standalone Rust lookahead limiter remains outside the permanent path because it introduced an asynchronous userspace bridge between MASTER and the hardware clock. If the product later permits arbitrary simultaneous streams, protection must instead be a graph-scheduled PipeWire processor after MASTER.
