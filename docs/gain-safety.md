# Gain safety invariants

The player never relies on positive digital or hardware gain to reach normal listening level.

Programme receivers are transports and remain at unity. MUSIC and MASTER are user/policy gain stages limited to unity or attenuation. The selected physical PipeWire sink is fixed at unity. Hardware mixer normalization is used only when ALSA exposes a trustworthy non-positive dB mapping; a control whose reported 0 dB resolves to 0% is treated as invalid and is not accepted as unity.

MUSIC and ALERT are mixed in PipeWire's floating-point graph. Their graph links remain at unity and priority is implemented by ducking MUSIC before announcement playback. The default MUSIC duck is -12 dB.

During the short interval in which an announcement is actually audible, the ALERT fader remains exactly at the user-selected value. If the normal MUSIC duck still leaves insufficient linear sample-peak budget, MUSIC alone is reduced further. Per channel, `music <= 1 - alert`; therefore the worst-case coherent sum is no greater than unity without playback policy taking ownership of the ALERT control.

At the defaults, full-scale MUSIC ducked by -12 dB contributes about 0.251 linear amplitude. An ALERT fader at approximately -2.51 dB contributes the complementary 0.749 and leaves the normal duck unchanged. If the user selects a louder ALERT level, MUSIC yields further while the file is audible; at true-unity ALERT, the mathematically safe worst-case MUSIC contribution is zero. This extra duck exists only while both buses may contribute.

All three permanent graph links carry absolute Pulse unity volume. Repeated startup, reconciliation and output switching are idempotent and cannot accumulate attenuation. Consequently, a single programme source at MUSIC=MASTER=100% reaches the physical software sink with gain exactly 1.0.

The retired standalone Rust lookahead limiter remains outside the permanent path because it introduced an asynchronous userspace bridge between MASTER and the hardware clock. If the product later permits arbitrary simultaneous streams, protection must instead be a graph-scheduled PipeWire processor after MASTER.
