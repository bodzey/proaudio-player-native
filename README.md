# ProAudio Player Native

Hardware-neutral Rust control plane for ProAudio Player.

Development is performed on `dev`. The `main` branch contains promoted stable revisions. The firmware repository is the composition layer that pins the native control plane and WebUI independently for a concrete appliance image.

## Responsibilities

The native daemon owns:

- alerts.in.ua polling through the official HTTPS API, Bearer-token authentication, `A/P/N` semantics, rate-limit handling and configurable clear confirmations;
- alert start/end announcements, ducking, mixer-state restoration and crash recovery;
- the daily minute-of-silence scheduler;
- exclusive arbitration between Spotify Connect, AirPlay, network audio, DLNA/UPnP and MPD/local playback;
- a unified player model and transport controls across MPD, MPRIS and DLNA;
- MPD library, playlists, queue and HTTP(S) stream playback;
- logical MUSIC, ALERT and MASTER gain control;
- physical-output discovery and selection behind a hardware-neutral routing interface;
- a versioned HTTP control API at `/api/v1`;
- standard UPnP MediaRenderer discovery and AVTransport/RenderingControl handling;
- atomic persistence of mutable runtime state.

Board-specific policy does not belong in this repository. Raspberry Pi, SoC, boot, storage-device and hardware-profile decisions are owned by `proaudio-player-firmware`.

## Runtime architecture

The native daemon is one Rust executable:

```text
proaudio-player-native
```

External media engines remain independent services. The current appliance uses Spotifyd, Shairport Sync, MPD, a UPnP/DLNA renderer engine, PipeWire with its Pulse compatibility server, ALSA, pacat and mpv. Keeping media engines outside the control process isolates protocol/decoder failures while the orchestration, state machine, API and scheduling remain native.

The control plane does not depend on a browser frontend. The WebUI is developed separately in `bodzey/proaudio-player-webui` and may be installed under `/usr/share/proaudio-player/webui`. If no frontend is installed, the control API and audio runtime continue to operate normally.

## Build

```bash
cargo build --release --locked
```

The release profile enables full LTO, a single codegen unit, symbol stripping and abort-on-panic. Architecture-specific CPU tuning is intentionally left to the firmware build so the source remains portable.

The resulting executable is:

```text
target/release/proaudio-player-native
```

The minimum supported Rust toolchain is 1.88.

## Run

```bash
proaudio-player-native --config /etc/proaudio-player-alert/config.yaml run
```

Available maintenance commands:

```text
once
status
test-start
test-end
test-silence
test-cycle --hold 5
```

For local frontend development:

```bash
PROAUDIO_WEBUI_DIR="/path/to/proaudio-player-webui/dist" \
  cargo run -- --config config/config.yaml.example run
```

## Runtime files

```text
/etc/proaudio-player-alert/config.yaml
/etc/proaudio-player-alert/audio.env
/var/lib/proaudio-player-alert/alerts-token
/var/lib/proaudio-player-alert/provider-settings.yaml
/var/lib/proaudio-player-alert/audio-settings.yaml
/var/lib/proaudio-player-alert/state.json
/usr/share/proaudio-player/announcements/alarm_start.mp3
/usr/share/proaudio-player/announcements/alarm_end.mp3
/usr/share/proaudio-player/announcements/minute_silence.mp3
/usr/share/proaudio-player/webui/                    # optional
```

The example configuration uses the official alerts.in.ua endpoint:

```text
https://api.alerts.in.ua/v1/iot/active_air_raid_alerts/{uid}.json
```

## Audio boundary

The current stable runtime controls PipeWire through the Pulse compatibility API. Source receivers remain transport endpoints at unity gain; user gain belongs to the logical MUSIC/ALERT/MASTER buses. Output hot-plug is isolated behind the logical MASTER bus and the parking sink so loss of a physical DAC does not destroy the control plane. Network audio senders can stream 48 kHz stereo float32 PCM into the MUSIC bus through `/api/v1/audio/network`; this ingress is transient and does not replace the permanent PipeWire graph.

See `docs/audio-architecture.md`, `docs/gain-safety.md` and `docs/verification.md` for the detailed audio contract.

## Frontend and API

New frontend code should target `/api/v1`. The unversioned `/api` path remains a compatibility alias.

`PROAUDIO_WEBUI_DIR` may override the optional frontend directory or disable static frontend delivery with `off`, `false`, `disabled`, `none`, `0` or an empty value.

See `docs/api.md` for the API contract.

## Factory announcement media

Factory alert and minute-silence files are stored read-only under `/usr/share/proaudio-player/announcements`. Mutable replacements and runtime state belong under `/var/lib`.

## License

The native control plane is proprietary software. See `LICENSE`.
