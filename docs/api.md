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
- `GET /library`, `POST /library/update`, `POST /library/play`
- `POST /streams/play`
- `GET /playlists`, `POST /playlists/load`
- `GET /queue`, `POST /queue/play`, `POST /queue/remove`, `POST /queue/clear`

## Errors

Errors are JSON objects with an `error` string. Invalid request bodies and explicitly validated input return 4xx responses; unavailable player services or devices return `503 Service Unavailable`. Clients must use HTTP status codes and must not parse localized error text.

## Frontend integration

A frontend is an optional API client and is not embedded in the native binary. For appliance compatibility, the daemon can serve an installed static frontend from `/usr/share/proaudio-player/webui`. This is only a delivery mechanism: if the directory is absent, `/` and frontend asset routes return 404 while `/api/v1`, `/api`, FourStream and UPnP endpoints remain operational.

`PROAUDIO_WEBUI_DIR` can point the compatibility host at another directory. Set it to `off`, `false`, `disabled`, `none`, `0`, or an empty value to disable frontend delivery explicitly.

A separately developed frontend should normally use the same origin or a reverse proxy to `/api/v1`. The API intentionally does not enable unrestricted cross-origin writes by default.
