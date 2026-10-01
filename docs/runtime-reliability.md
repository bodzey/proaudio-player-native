# Runtime recovery and shutdown

The source arbiter first mutes losing and duplicate streams, then normalizes
the winning receiver to unity gain before unmuting it. A failed mute or gain
operation leaves the new winner muted when the backend permits it and keeps
the selection pending for reconciliation. Source status changes only after
the audio operations succeed. User gain and alert ducking remain on MUSIC.

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

Regression coverage includes source mute/gain failures, duplicate streams,
continuous fader updates, filesystem recovery and daemon termination with both
signals. `bash tests/test_shutdown.sh` uses a missing Pulse socket and a mock
MPD command, so it does not require or control an audio device.
