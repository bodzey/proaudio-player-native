# ProAudio Player Native

Native Rust control plane for ProAudio Player. The current behavior-parity reference is `bodzey/proaudio_player` branch `feature/4stream-integration` at `564f4be0a4b4ef5d63f8c3c4703f1d2e42ee138c`.

Development lives on `dev`. `main` is reserved for promoted/stable snapshots.

## Current parity target

The native daemon keeps the existing runtime contract so it can be introduced into the current Buildroot firmware without changing the proven audio engines around it:

- `alerts.in.ua` polling with the DEV endpoint, Bearer token, `A/P/N` semantics, rate-limit handling and configurable clear confirmations;
- alert start/end announcements, ducking, restore snapshots and crash recovery;
- daily minute-of-silence scheduler in `Europe/Kyiv`, catch-up window and alert pre-emption;
- exclusive source arbitration for Spotify Connect, AirPlay, DLNA/UPnP and MPD/local playback;
- resilient arbitration heartbeat every two seconds in addition to PipeWire/Pulse stream/sink events;
- explicit local-source switching: selecting MPD/local content may replace an active external music source while priority alerts still block music controls;
- MPRIS metadata and transport control for spotifyd and Shairport Sync;
- MPD library, playlists, queue and HTTP(S) stream URL playback;
- 0..150% software gain, physical output/alert-bus controls and ALSA hardware mixer diagnostics;
- current Web UI and its HTTP API contract;
- LinkPlay/4STREAM compatibility API (`/httpapi.asp`), metadata/status commands and `_linkplay._tcp` discovery descriptor;
- UPnP MediaRenderer description, SSDP M-SEARCH/alive discovery and core SOAP AVTransport/RenderingControl actions;
- the same YAML configuration, override files, token file and JSON runtime state paths as the Python implementation.

No Python runtime is required by this project. The first parity implementation intentionally keeps the same external media engines and Linux audio utilities as the current firmware (`spotifyd`, Shairport Sync, gmediarender/GStreamer, MPD, PipeWire/Pulse compatibility, ALSA and mpv). This limits migration risk while moving orchestration, state machine, HTTP API, scheduling and the 4STREAM gateway into one native process.

## Build

```bash
cargo build --release
```

The minimum supported Rust toolchain is 1.88 because the locked dependency
graph includes ICU 2.3 in addition to stable Cargo Edition 2024 manifests.

The resulting binary is:

```text
target/release/proaudio-player-native
```

For the appliance image, use the `native` branch of `bodzey/proaudio-player-firmware` and its `proaudio_rpi4_64_native_defconfig` instead of building the target binary manually on Raspberry Pi.

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

The repository first targets functional parity. After hardware validation, command adapters (`pactl`, `amixer`, `mpc`, `busctl`) can be replaced incrementally by direct PipeWire, ALSA, MPD protocol and D-Bus integrations without changing the state machine or Web/4STREAM API contracts.

## Factory announcement media

Factory alert and minute-silence MP3 files are owned by this repository under `assets/announcements/` and are installed read-only under `/usr/share/proaudio-player/announcements/`. Runtime state remains under `/var/lib`; firmware no longer depends on the legacy Python player repository.
