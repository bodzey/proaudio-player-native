# ProAudio Player Native

Native Rust control plane for ProAudio Player. This repository is a behavior-compatible rewrite of `bodzey/proaudio_player` branch `dev` at `7a391090947ebec24c45667a5a7d4bd82d5d5302`.

Development lives on `dev`. `main` is reserved for promoted/stable snapshots.

## Current parity target

The native daemon keeps the existing runtime contract so it can be introduced into the current Buildroot firmware without changing the audio services around it:

- `alerts.in.ua` polling with the DEV endpoint, Bearer token, `A/P/N` semantics, rate-limit handling and configurable clear confirmations;
- alert start/end announcements, ducking, restore snapshots and crash recovery;
- daily 09:00 minute-of-silence scheduler in `Europe/Kyiv`, catch-up window and alert pre-emption;
- source arbitration for Spotify Connect, AirPlay, DLNA/UPnP and MPD/local playback;
- MPRIS metadata and transport control for spotifyd and Shairport Sync;
- MPD library, playlists and queue control;
- 0..150% software gain, physical output/alert-bus controls and ALSA hardware mixer diagnostics;
- current Web UI and its HTTP API contract;
- LinkPlay/4STREAM compatibility endpoints and SSDP discovery;
- the same YAML configuration, override files, token file and JSON runtime state paths as the Python implementation.

No Python runtime is required by this project. The first parity implementation intentionally keeps the same external media engines and Linux audio utilities as the current firmware (`spotifyd`, Shairport Sync, gmediarender/GStreamer, MPD, PipeWire/Pulse compatibility, ALSA and mpv). This limits migration risk while moving the orchestration, state machine, HTTP API, scheduling and 4STREAM gateway into one native process.

## Build

```bash
cargo build --release
```

The resulting binary is:

```text
target/release/proaudio-player-native
```

## Run on the current player filesystem

```bash
proaudio-player-native --config /etc/proaudio-player-alert/config.yaml run
```

Other compatibility commands:

```text
once
status
test-start
test-end
test-silence
test-cycle --hold 5
```

## Runtime files

```text
/etc/proaudio-player-alert/config.yaml
/etc/proaudio-player-alert/alerts-token
/var/lib/proaudio-player-alert/provider-settings.yaml
/var/lib/proaudio-player-alert/audio-settings.yaml
/var/lib/proaudio-player-alert/state.json
/var/lib/proaudio-player-alert/media/alarm_start.mp3
/var/lib/proaudio-player-alert/media/alarm_end.mp3
/var/lib/proaudio-player-alert/media/minute_silence.mp3
```

The DEV example configuration points to:

```text
http://192.168.88.122/v1/iot/active_air_raid_alerts/{uid}.json
```

## Migration direction

The repository first targets functional parity. After hardware validation, command adapters (`pactl`, `amixer`, `mpc`, `busctl`) can be replaced incrementally by direct PipeWire, ALSA, MPD protocol and D-Bus integrations without changing the state machine or Web API.
