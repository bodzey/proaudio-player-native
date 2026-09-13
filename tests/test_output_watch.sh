#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
test_root="$(mktemp -d)"
trap 'rm -rf -- "$test_root"' EXIT INT TERM

export MOCK_PACTL_STATE="$test_root/state"
export XDG_RUNTIME_DIR="$test_root/run"
mkdir -p "$MOCK_PACTL_STATE" "$XDG_RUNTIME_DIR" "$test_root/bin"
ln -s "$repo_root/tests/fixtures/pactl" "$test_root/bin/pactl"
export PATH="$test_root/bin:/usr/bin:/bin"

state_file="$XDG_RUNTIME_DIR/proaudio-player-bus-modules"
cat >"$state_file" <<'EOF'
PHYSICAL=usb_output
OUTPUT_TARGET=usb_output
OUTPUT_LOOP_MODULE=7
MUSIC_LOOP_MODULE=5
ALERT_LOOP_MODULE=6
EOF

cat >"$MOCK_PACTL_STATE/sinks" <<'EOF'
1	onboard_output	PipeWire	float32le 2ch 48000Hz	RUNNING
2	usb_output	PipeWire	float32le 2ch 48000Hz	SUSPENDED
EOF

output="$({ AUDIO_ENV=/nonexistent OUTPUT_ENV=/nonexistent BUS_SCRIPT=/bin/true \
    bash "$repo_root/scripts/proaudio-player-output-watch" once; } 2>&1)"
grep -Fq 'Audio routing: reconciling usb_output -> usb_output' <<<"$output"
! grep -Fq -- '-> onboard_output' <<<"$output"

cat >"$MOCK_PACTL_STATE/sinks" <<'EOF'
1	onboard_output	PipeWire	float32le 2ch 48000Hz	RUNNING
EOF

output="$({ AUDIO_ENV=/nonexistent OUTPUT_ENV=/nonexistent BUS_SCRIPT=/bin/true \
    bash "$repo_root/scripts/proaudio-player-output-watch" once; } 2>&1)"
grep -Fq 'Audio routing: reconciling usb_output -> onboard_output' <<<"$output"

printf 'output watcher shell tests passed\n'
