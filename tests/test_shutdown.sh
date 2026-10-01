#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
work="$(mktemp -d)"
player_pid=""
cleanup() {
    if [[ -n "$player_pid" ]]; then
        kill -KILL "$player_pid" 2>/dev/null || true
        wait "$player_pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

cargo build --locked
player="${CARGO_TARGET_DIR:-target}/debug/proaudio-player-native"
mkdir "$work/bin"
printf '#!/bin/sh\nexit 0\n' >"$work/bin/mpc"
chmod +x "$work/bin/mpc"
cat >"$work/config.yaml" <<EOF
state_file: "$work/state.json"
audio:
  notifications_enabled: false
minute_silence:
  enabled: false
api:
  enabled: false
EOF

for stop_signal in TERM INT; do
    rm -f "$work/state.json"
    PATH="$work/bin:$PATH" PULSE_SERVER="unix:$work/missing-pulse" RUST_LOG=info \
        "$player" --config "$work/config.yaml" run >"$work/player.log" 2>&1 &
    player_pid=$!
    ready=false
    for ((attempt = 0; attempt < 100; attempt++)); do
        if grep -Fq 'ProAudio Player native control plane started' "$work/player.log"; then
            ready=true
            break
        fi
        if ! kill -0 "$player_pid" 2>/dev/null; then
            cat "$work/player.log" >&2
            echo 'native daemon failed to start' >&2
            exit 1
        fi
        sleep 0.05
    done
    if [[ "$ready" != true ]]; then
        cat "$work/player.log" >&2
        echo 'native daemon startup timed out' >&2
        exit 1
    fi

    kill -s "$stop_signal" "$player_pid"
    for ((attempt = 0; attempt < 100; attempt++)); do
        if ! kill -0 "$player_pid" 2>/dev/null; then
            break
        fi
        sleep 0.05
    done
    if kill -0 "$player_pid" 2>/dev/null; then
        cat "$work/player.log" >&2
        echo "native daemon did not stop after SIG$stop_signal" >&2
        exit 1
    fi
    wait "$player_pid"
    player_pid=""
    grep -Fq '"mode": "normal"' "$work/state.json"
    grep -Fq "Отримано SIG$stop_signal" "$work/player.log"
done

echo 'Native daemon shutdown and durable state checks passed'
