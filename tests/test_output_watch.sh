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


cat >"$MOCK_PACTL_STATE/sinks" <<'EOF'
1	onboard_output	PipeWire	float32le 2ch 48000Hz	RUNNING
2	usb_output	PipeWire	float32le 2ch 48000Hz	SUSPENDED
EOF

cat >"$state_file" <<'EOF'
PHYSICAL=usb_output
OUTPUT_TARGET=usb_output
OUTPUT_LOOP_MODULE=7
MUSIC_LOOP_MODULE=5
ALERT_LOOP_MODULE=6
EOF

output_env="$test_root/audio-output.env"
printf 'PHYSICAL_SINK=usb_output\n' >"$output_env"
printf '2\n' >"$MOCK_PACTL_STATE/subscribe-hold"
apply_log="$test_root/applied-output"
bus_script="$test_root/mock-bus.sh"
cat >"$bus_script" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "${PHYSICAL_SINK:-AUTO}" >>"${MOCK_APPLY_LOG:?}"
EOF
chmod +x "$bus_script"

MOCK_APPLY_LOG="$apply_log" AUDIO_ENV=/nonexistent OUTPUT_ENV="$output_env" BUS_SCRIPT="$bus_script" \
    bash "$repo_root/scripts/proaudio-player-output-watch" >"$test_root/watch.log" 2>&1 &
watch_pid=$!
sleep 0.2
printf 'PHYSICAL_SINK=onboard_output\n' >"$output_env"
wait "$watch_pid"

grep -Fq 'onboard_output' "$apply_log"

printf 'output watcher runtime selection test passed\n'
