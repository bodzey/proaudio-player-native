# ProAudio Player control API

The native daemon exposes a hardware-neutral HTTP control API. The API is part of the player control plane; a browser frontend is optional. Alerts, source arbitration, playback control, audio routing and standard UPnP/DLNA continue to work when no WebUI is installed.

## Versioning

The stable API is available below `/api/v1`. Existing `/api` URLs remain as a compatibility alias. New frontend development should target `/api/v1` and inspect `GET /api/v1/capabilities`.

Removing or reinterpreting fields requires a new major URL version. New optional fields and endpoints may be added within version 1.

## Discovery and health

- `GET /api/v1/health` — process health and API version.
- `GET /api/v1/capabilities` — API version, event transport and features.
- `GET /api/v1/status` — complete current player state.
- `GET /api/v1/events` — low-rate status Server-Sent Events.
- `GET /api/v1/meters` — demand-driven 50 Hz stereo sample-peak/RMS/clip metering for MASTER, MUSIC and ALERT.

The meter path reads monitor streams exposed by the PipeWire Pulse compatibility server. It measures PCM signal amplitude; mixer gain values are not substituted for signal level.

Peak values are sample peaks over each 20 ms analysis window in dBFS. RMS is calculated per channel over the same samples. The clip flag reports effectively full-scale samples; this is not oversampled true-peak/dBTP metering.

## Player contract

`status.player` is the frontend-facing representation of the active player regardless of transport. MPD/local playback, Spotify Connect, AirPlay and DLNA/UPnP use the same player shape and capability-driven controls.

Protocol-specific mechanisms remain inside the backend. Clients should enable actions from the returned `controls` object rather than branch on transport names.

## Control resources

The following paths are relative to `/api/v1`:

- `POST /volume`, `POST /mute`, `POST /player`
- `GET|POST /audio/mixer`, `GET|POST /audio/outputs`
- `POST /audio/network` — one live network PCM programme stream
- `GET /audio/hardware`, `GET /audio/diagnostics`, `POST /audio/level`
- `GET|PUT /settings/audio`, `GET|PUT /settings/alerts`
- `POST /settings/alerts/test`
- `GET /settings/alerts/media`
- `PUT /settings/alerts/media/{kind}`
- `DELETE /settings/alerts/media/{kind}`
- `GET /library`, `POST /library/update`, `POST /library/play`
- `POST /streams/play`
- `GET /radio/stations`
- `GET /playlists`, `POST /playlists/load`
- `GET /queue`, `POST /queue/play`, `POST /queue/remove`, `POST /queue/clear`

Alert-media uploads accept the raw MP3 body for `alarm_start`, `alarm_end` or `minute_silence`, up to 16 MiB. Runtime replacements are stored at the configured persistent paths; factory copies remain read-only below `/usr/share/proaudio-player/announcements`.

`GET|PUT /settings/audio` controls the independent air-raid and minute-of-silence features. Runtime changes are persisted atomically and observed without restarting the daemon.

## Network audio ingress

`POST /api/v1/audio/network` accepts one live PCM programme stream at a time.
It is intended for trusted local senders such as the ProAudio Player Android app
or Chrome extension.

The wire format is fixed and deliberately simple:

- `Content-Type: application/x-proaudio-pcm`
- `X-ProAudio-Sample-Format: float32le`
- `X-ProAudio-Sample-Rate: 48000`
- `X-ProAudio-Channels: 2`
- request body: interleaved little-endian stereo float32 PCM

The daemon forwards the body to a unity-gain `pacat` playback stream targeting
the logical MUSIC sink. Source arbitration, alert ducking, MUSIC gain, MASTER gain
and physical output routing therefore remain identical to the other programme
transports. A second simultaneous network stream returns `409 Conflict`.

Disconnecting the HTTP request terminates the transient playback stream. The
runtime image must provide `pacat` from the PulseAudio client utilities.

## UPnP/DLNA

The daemon exposes a standard UPnP MediaRenderer surface for DLNA clients:

- `GET /upnp/device.xml`
- `GET /description.xml`
- `GET /upnp/avtransport.xml`
- `GET /upnp/renderingcontrol.xml`
- `POST /upnp/control`
- SSDP M-SEARCH and alive announcements on UDP 1900.

Only standard AVTransport and RenderingControl services are advertised; vendor-specific compatibility gateways are intentionally excluded.

## Errors

API errors are JSON objects with an `error` string. Invalid input returns 4xx responses; unavailable player services or devices return `503 Service Unavailable`. Clients should use HTTP status codes and not parse localized error strings.

## Frontend integration

A frontend is an optional API client and is not embedded in the Rust executable. For appliance compatibility, the daemon can serve an installed frontend from `/usr/share/proaudio-player/webui`. If the directory is absent, frontend routes return 404 while the control API and UPnP endpoints remain operational.

`PROAUDIO_WEBUI_DIR` may point to another frontend directory or disable frontend delivery explicitly.

A separately developed frontend should normally use the same origin or a trusted reverse proxy to `/api/v1`. The native API does not enable unrestricted CORS.
