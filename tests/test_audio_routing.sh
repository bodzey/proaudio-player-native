#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
test_root="$(mktemp -d)"
trap 'rm -rf -- "$test_root"' EXIT INT TERM

new_case() {
    local name="$1"
    export MOCK_PACTL_STATE="$test_root/$name/state"
    export XDG_RUNTIME_DIR="$test_root/$name/run"
    mkdir -p "$MOCK_PACTL_STATE" "$XDG_RUNTIME_DIR" "$test_root/$name/bin"
    : >"$MOCK_PACTL_STATE/modules"
    : >"$MOCK_PACTL_STATE/unloaded"
    : >"$MOCK_PACTL_STATE/calls"
    ln -s "$repo_root/tests/fixtures/pactl" "$test_root/$name/bin/pactl"
    export PATH="$test_root/$name/bin:/usr/bin:/bin"
}

new_case no_dac
HARDWARE_MIXER_MODE=off bash "$repo_root/scripts/audio-buses.sh" start
state_file="$XDG_RUNTIME_DIR/proaudio-player-bus-modules"
grep -Fxq 'PHYSICAL=' "$state_file"
grep -Fxq 'OUTPUT_TARGET=proaudio_player_parking' "$state_file"
grep -Eq '^PARKING_BUS_MODULE=[0-9]+$' "$state_file"
grep -Fq 'module-loopback|source=proaudio_player_master.monitor sink=proaudio_player_parking' \
    "$MOCK_PACTL_STATE/modules"
[[ "$(grep -c '^set-sink-input-volume|.* 65536$' "$MOCK_PACTL_STATE/calls")" == 3 ]]

touch "$MOCK_PACTL_STATE/physical-present"
HARDWARE_MIXER_MODE=off bash "$repo_root/scripts/audio-buses.sh" switch
grep -Fxq 'PHYSICAL=mock_physical' "$state_file"
grep -Fxq 'OUTPUT_TARGET=mock_physical' "$state_file"
grep -Fxq '7' "$MOCK_PACTL_STATE/unloaded"
grep -Fq 'module-loopback|source=proaudio_player_master.monitor sink=mock_physical' \
    "$MOCK_PACTL_STATE/modules"

new_case partial_failure
printf '4\n' >"$MOCK_PACTL_STATE/fail-at"
if HARDWARE_MIXER_MODE=off bash "$repo_root/scripts/audio-buses.sh" start; then
    echo 'partial graph construction unexpectedly succeeded' >&2
    exit 1
fi
[[ ! -e "$XDG_RUNTIME_DIR/proaudio-player-bus-modules" ]]
[[ "$(paste -sd, "$MOCK_PACTL_STATE/unloaded")" == '3,2,1' ]]

printf 'audio routing shell tests passed\n'
