#!/usr/bin/env bash
set -euo pipefail

MUSIC_SINK="${MUSIC_SINK:-proaudio_player_music}"
ALERT_SINK="${ALERT_SINK:-proaudio_player_alert}"
PHYSICAL_SINK="${PHYSICAL_SINK:-AUTO}"
LOOPBACK_LATENCY_MSEC="${LOOPBACK_LATENCY_MSEC:-100}"
OUTPUT_VOLUME_PERCENT="${OUTPUT_VOLUME_PERCENT:-100}"
SINK_WAIT_SECONDS="${SINK_WAIT_SECONDS:-30}"
STATE_FILE="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is not set}/proaudio-player-bus-modules"

find_physical_sink() {
    if [[ "$PHYSICAL_SINK" != "AUTO" ]]; then
        if pactl get-sink-volume "$PHYSICAL_SINK" >/dev/null 2>&1; then
            printf '%s\n' "$PHYSICAL_SINK"
            return 0
        fi
        return 1
    fi
    pactl list short sinks | awk -v music="$MUSIC_SINK" -v alert="$ALERT_SINK" '
        $2 != music && $2 != alert && $2 != "auto_null" &&
        $2 !~ /^proaudio_player_/ {
            if ($2 ~ /^alsa_output\.usb-/ && usb == "") usb=$2
            else if ($2 ~ /^alsa_output\./ && alsa == "") alsa=$2
            else if (other == "") other=$2
        }
        END {
            if (usb != "") print usb
            else if (alsa != "") print alsa
            else if (other != "") print other
        }'
}

wait_for_physical_sink() {
    local deadline=$((SECONDS + SINK_WAIT_SECONDS))
    local physical=""
    while ((SECONDS <= deadline)); do
        physical="$(find_physical_sink || true)"
        if [[ -n "$physical" ]]; then
            printf '%s\n' "$physical"
            return 0
        fi
        sleep 1
    done
    if [[ "$PHYSICAL_SINK" == "AUTO" ]]; then
        echo "Не знайдено фізичного аудіовиходу протягом ${SINK_WAIT_SECONDS} с" >&2
    else
        echo "Аудіовихід '$PHYSICAL_SINK' не з'явився протягом ${SINK_WAIT_SECONDS} с" >&2
    fi
    return 1
}

unload_saved_modules() {
    [[ -f "$STATE_FILE" ]] || return 0

    local modules=()
    local key value index
    while IFS='=' read -r key value; do
        if [[ "$key" == "MODULE" && "$value" =~ ^[0-9]+$ ]]; then
            modules+=("$value")
        fi
    done <"$STATE_FILE"

    for ((index=${#modules[@]} - 1; index >= 0; index--)); do
        pactl unload-module "${modules[index]}" >/dev/null 2>&1 || true
    done
    rm -f -- "$STATE_FILE"
}

start_buses() {
    unload_saved_modules
    local physical
    physical="$(wait_for_physical_sink)"
    if ! [[ "$OUTPUT_VOLUME_PERCENT" =~ ^[0-9]+$ ]] \
        || ((10#$OUTPUT_VOLUME_PERCENT > 100)); then
        echo "OUTPUT_VOLUME_PERCENT має бути цілим числом від 0 до 150" >&2
        exit 1
    fi
    : >"$STATE_FILE"
    printf 'PHYSICAL=%s\n' "$physical" >>"$STATE_FILE"

    # Завдяки api.alsa.soft-mixer це лише програмний рівень PipeWire.
    # Апаратні PCM/mute регулятори ALSA залишаються без змін.
    pactl set-sink-volume "$physical" "${OUTPUT_VOLUME_PERCENT}%"
    pactl set-sink-mute "$physical" 0

    local music_module alert_module music_loop alert_loop
    music_module="$(pactl load-module module-null-sink \
        sink_name="$MUSIC_SINK" \
        sink_properties="device.description=proaudio_player_music_Bus" \
        rate=48000 channels=2)"
    printf 'MODULE=%s\n' "$music_module" >>"$STATE_FILE"

    alert_module="$(pactl load-module module-null-sink \
        sink_name="$ALERT_SINK" \
        sink_properties="device.description=proaudio_player_alert_Bus" \
        rate=48000 channels=2)"
    printf 'MODULE=%s\n' "$alert_module" >>"$STATE_FILE"

    music_loop="$(pactl load-module module-loopback \
        source="$MUSIC_SINK.monitor" sink="$physical" \
        latency_msec="$LOOPBACK_LATENCY_MSEC" \
        source_dont_move=true sink_dont_move=true)"
    printf 'MODULE=%s\n' "$music_loop" >>"$STATE_FILE"

    alert_loop="$(pactl load-module module-loopback \
        source="$ALERT_SINK.monitor" sink="$physical" \
        latency_msec="$LOOPBACK_LATENCY_MSEC" \
        source_dont_move=true sink_dont_move=true)"
    printf 'MODULE=%s\n' "$alert_loop" >>"$STATE_FILE"

    pactl set-default-sink "$MUSIC_SINK"
    pactl set-sink-volume "$MUSIC_SINK" 100%
    pactl set-sink-volume "$ALERT_SINK" 100%
    pactl set-sink-mute "$MUSIC_SINK" 0
    pactl set-sink-mute "$ALERT_SINK" 0
    echo "Музична й службова шини підключені до $physical; програмний вихід ${OUTPUT_VOLUME_PERCENT}%"
}

stop_buses() {
    local physical=""
    if [[ -f "$STATE_FILE" ]]; then
        physical="$(awk -F= '$1 == "PHYSICAL" { print substr($0, index($0,"=")+1); exit }' "$STATE_FILE")"
    fi
    unload_saved_modules
    if [[ -n "$physical" ]]; then
        pactl set-default-sink "$physical" >/dev/null 2>&1 || true
    fi
}

case "${1:-}" in
    start) start_buses ;;
    stop) stop_buses ;;
    restart) stop_buses; start_buses ;;
    *) echo "Використання: $0 {start|stop|restart}" >&2; exit 2 ;;
esac
