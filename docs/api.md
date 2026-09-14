# ProAudio Player control API

The native daemon exposes a hardware-neutral HTTP control API. The API is part of the player control plane; a browser frontend is not. Alerts, source arbitration, playback control, audio routing and compatibility protocols continue to work when no Web UI is installed.

## Compatibility

The stable API is available below `/api/v1`. Existing `/api` URLs remain as a compatibility alias during migration. New frontend development should target `/api/v1` and inspect `GET /api/v1/capabilities` instead of inferring features from a UI version.

API changes that remove or reinterpret fields require a new major URL version. New optional response fields and new endpoints may be added within version 1.

## Discovery and health

- `GET /api/v1/health` — process health and API version.
- `GET /api/v1/capabilities` — API version, event transport and features.
- `GET /api/v1/status` — complete current player state.
- `GET /api/v1/events` — Server-Sent Events stream. Status events use the `status` event name and contain the same JSON document as the status endpoint.
- `GET /api/v1/meters` — high-rate Server-Sent Events stream for real signal metering. `meter` events contain 25 Hz stereo Peak/RMS/clip snapshots for `master`, `music` and `alert`. Capture is activated only while at least one meter client is connected.

The meter path reads the monitor streams exposed by the PipeWire Pulse compatibility server. It measures actual PCM signal amplitude; mixer gain settings are never substituted for signal level.

## Player contract

`status.player` is the frontend-facing representation of the active player regardless of transport. MPD/local playback, Spotify Connect, AirPlay and DLNA/UPnP expose the same player shape and the same capability-driven transport controls. Protocol-specific control mechanisms remain inside the backend.

The current fields include source/backend identity, playback state, metadata, position/duration/progress and a `controls` object. Clients must enable actions from `controls` instead of branching on a particular protocol.

## Control resources

The following paths are relative to `/api/v1`:

- `POST /volume`, `POST /mute`, `POST /player`
- `GET|POST /audio/mixer`, `GET|POST /audio/outputs`
- `GET|POST /audio/hardware`, `POST /audio/level`
- `GET|PUT /settings/audio`, `GET|PUT /settings/alerts`
- `POST /settings/alerts/test`
- `GET /settings/alerts/media` — metadata for the alarm-start, alarm-end and minute-silence MP3 files.
- `PUT /settings/alerts/media/{kind}` — atomically replace one file with a raw MP3 body (`alarm_start`, `alarm_end` or `minute_silence`, up to 16 MiB).
- `DELETE /settings/alerts/media/{kind}` — restore the selected factory MP3.
- `GET /library`, `POST /library/update`, `POST /library/play`
- `POST /streams/play`
- `GET /playlists`, `POST /playlists/load`
- `GET /queue`, `POST /queue/play`, `POST /queue/remove`, `POST /queue/clear`

`GET|PUT /settings/audio` also controls `notifications_enabled` and the complete minute-silence schedule: `minute_silence_enabled`, `minute_silence_start_time`, `minute_silence_timezone`, `minute_silence_catch_up_seconds` and `minute_silence_music_fade_seconds`. Changes are persisted and observed by the running alert controller without restarting the daemon. Disabling all notifications stops provider polling, cancels an active priority mode and restores the saved music level.

Uploaded alert media is stored at the configured active paths. Appliance images configure these below `/var/lib/proaudio-player-alert/media`, which is backed by the persistent DATA partition. Factory copies remain read-only below `/usr/share/proaudio-player/announcements`.

## Errors

Errors are JSON objects with an `error` string. Invalid request bodies and explicitly validated input return 4xx responses; unavailable player services or devices return `503 Service Unavailable`. Clients must use HTTP status codes and must not parse localized error text.

## Frontend integration

A frontend is an optional API client and is not embedded in the native binary. For appliance compatibility, the daemon can serve an installed static frontend from `/usr/share/proaudio-player/webui`. This is only a delivery mechanism: if the directory is absent, `/` and frontend asset routes return 404 while `/api/v1`, `/api`, FourStream and UPnP endpoints remain operational.

`PROAUDIO_WEBUI_DIR` can point the compatibility host at another directory. Set it to `off`, `false`, `disabled`, `none`, `0`, or an empty value to disable frontend delivery explicitly.

A separately developed frontend should normally use the same origin or a reverse proxy to `/api/v1`. The API intentionally does not enable unrestricted cross-origin writes by default.
