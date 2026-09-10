# ProAudio Player control API

The native daemon exposes a hardware-neutral HTTP control API. A frontend is
an optional client: no HTML, CSS, JavaScript, manifest, icon, or service worker
is required to compile or run the player.

## Compatibility

The stable API is available below `/api/v1`. Existing `/api` URLs remain as a
compatibility alias during migration. Clients should inspect
`GET /api/v1/capabilities` and must not infer features from a UI version.

API changes that remove or reinterpret fields require a new major URL version.
New optional response fields and new endpoints may be added within version 1.

## Discovery and health

- `GET /api/v1/health` — process health and API version.
- `GET /api/v1/capabilities` — API version, event transport, and features.
- `GET /api/v1/status` — complete current player state.
- `GET /api/v1/events` — Server-Sent Events stream. Status events use the
  `status` event name and contain the same JSON document as the status endpoint.

## Control resources

- `POST /volume`, `POST /mute`, `POST /player`
- `GET|POST /audio/mixer`, `GET|POST /audio/outputs`
- `GET|POST /audio/hardware`, `POST /audio/level`
- `GET|PUT /settings/audio`, `GET|PUT /settings/alerts`
- `POST /settings/alerts/test`
- `GET /library`, `POST /library/update`, `POST /library/play`
- `POST /streams/play`
- `GET /playlists`, `POST /playlists/load`
- `GET /queue`, `POST /queue/play`, `POST /queue/remove`, `POST /queue/clear`

The paths in this section are relative to `/api/v1`.

## Errors

Errors are JSON objects with an `error` string. Invalid input returns a 4xx
status; unavailable player services or devices return `503 Service Unavailable`.
Clients must use HTTP status codes and must not parse localized error text.

## Frontend integration

Production frontends should use the player origin or a reverse proxy. A
separately hosted development frontend should proxy `/api/v1` to the device.
The API intentionally does not enable unrestricted cross-origin writes.
