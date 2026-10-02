# Runtime recovery and shutdown

The source arbiter first mutes losing and duplicate streams, then normalizes
the winning receiver to unity gain before unmuting it. A failed mute or gain
operation leaves the new winner muted when the backend permits it and keeps
the selection pending for reconciliation. Source status changes only after
the audio operations succeed. User gain and alert ducking remain on MUSIC.

Pulse control requests have a six second deadline covering both queueing and
execution. The worker skips queued requests whose caller has cancelled or whose
deadline has elapsed. A request already dispatched to the server may have taken
effect before the caller times out; cancellation cannot undo that operation.

Every established Pulse connection has a new epoch. Requests queued for a prior
connection are rejected instead of being replayed against reused sink-input
indices. The source arbiter discards its selection and suppression history when
the epoch changes, rejects discovery spanning two connections, and binds stream
mutations to the connection that supplied their indices. Ordinary query failures
within the same connection keep the suppression history intact. Mixer recovery
also includes the epoch, so saved gain is restored even when a restarted server
assigns the same indices to MUSIC, ALERT and MASTER.

Mixer changes are coalesced for 750 ms after the last update. Continuous
control traffic can delay a write attempt for at most two seconds. These are
scheduling limits; a slow or unavailable filesystem can delay durable storage.
Failed writes are retried every second without needing another client update.
Background writes and explicit flushes share a lock and snapshot the current
settings after acquiring it. Unchanged settings do not replace the file.

SIGTERM and SIGINT stop the daemon's runtime tasks and mixer writer, then flush
the current mixer and alert runtime state. The final async flush has a five
second deadline; the systemd unit has a ten second stop timeout for stalled
processes or blocking I/O. A flush failure is logged and produces an error exit.

Persistent files use the existing temporary-file, fsync, rename and parent-fsync
sequence. SIGKILL or physical power loss cannot execute the shutdown flush, so
the most recent unsaved changes can still be lost. Storage hardware, filesystem
mount policy and the firmware image need separate power-cut testing.

Regression coverage includes cancelled and expired Pulse requests, simulated
reconnections with reused indices, source mute/gain failures, duplicate streams,
continuous fader updates, filesystem recovery and daemon termination with both
signals. `bash tests/test_shutdown.sh` uses a missing Pulse socket and a mock
MPD command, so it does not require or control an audio device.
