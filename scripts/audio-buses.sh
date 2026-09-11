#!/usr/bin/env bash
set -euo pipefail

MUSIC_SINK="${MUSIC_SINK:-proaudio_player_music}"
ALERT_SINK="${ALERT_SINK:-proaudio_player_alert}"
PHYSICAL_SINK="${PHYSICAL_SINK:-AUTO}"
SAMPLE_RATE="${SAMPLE_RATE:-48000}"
LOOPBACK_LATENCY_MSEC="${LOOPBACK_LATENCY_MSEC:-100}"
OUTPUT_VOLUME_PERCENT="${OUTPUT_VOLUME_PERCENT:-100}"
HARDWARE_MIXER_MODE="${HARDWARE_MIXER_MODE:-unity}"
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
    echo "Аудіовихід '$PHYSICAL_SINK' не з'явився протягом ${SINK_WAIT_SECONDS} с" >&2
    return 1
}

state_value() {
    local wanted="$1"
    [[ -f "$STATE_FILE" ]] || return 0
    awk -F= -v wanted="$wanted" '$1 == wanted { print substr($0, index($0,"=")+1); exit }' "$STATE_FILE"
}

write_state() {
    local physical="$1" music_bus="$2" alert_bus="$3" music_loop="$4" alert_loop="$5"
    local temporary="${STATE_FILE}.tmp"
    {
        printf 'PHYSICAL=%s\n' "$physical"
        printf 'MUSIC_BUS_MODULE=%s\n' "$music_bus"
        printf 'ALERT_BUS_MODULE=%s\n' "$alert_bus"
        printf 'MUSIC_LOOP_MODULE=%s\n' "$music_loop"
        printf 'ALERT_LOOP_MODULE=%s\n' "$alert_loop"
    } >"$temporary"
    mv -f -- "$temporary" "$STATE_FILE"
}

unload_saved_modules() {
    [[ -f "$STATE_FILE" ]] || return 0
    local modules=()
    local key value index
    while IFS='=' read -r key value; do
        if [[ "$key" == "MODULE" || "$key" == *_MODULE ]] && [[ "$value" =~ ^[0-9]+$ ]]; then
            modules+=("$value")
        fi
    done <"$STATE_FILE"
    for ((index=${#modules[@]} - 1; index >= 0; index--)); do
        pactl unload-module "${modules[index]}" >/dev/null 2>&1 || true
    done
    rm -f -- "$STATE_FILE"
}

alsa_card_for_sink() {
    local physical="$1"
    pactl list sinks | awk -v wanted="$physical" '
        /^Sink #[0-9]+/ { active=0; next }
        /^[[:space:]]*Name:/ { active=($2 == wanted); next }
        active && ($1 == "alsa.card" || $1 == "api.alsa.card") && $2 == "=" {
            gsub(/"/, "", $3)
            if ($3 ~ /^[0-9]+$/) {
                print $3
                exit
            }
        }'
}

safe_playback_control() {
    local name="${1,,}"
    case "$name" in
        *capture*|*mic*|*boost*|*gain*|*input*|*adc*|*loopback*|*monitor*|*tone*|*bass*|*treble*)
            return 1
            ;;
    esac
    return 0
}

prepare_hardware_mixer() {
    local physical="$1"
    [[ "$HARDWARE_MIXER_MODE" == "off" ]] && return 0
    if [[ "$HARDWARE_MIXER_MODE" != "unity" ]]; then
        echo "HARDWARE_MIXER_MODE має бути 'unity' або 'off'" >&2
        return 1
    fi
    command -v amixer >/dev/null 2>&1 || return 0

    local card control details after applied=0
    card="$(alsa_card_for_sink "$physical" || true)"
    [[ "$card" =~ ^[0-9]+$ ]] || return 0

    while IFS= read -r control; do
        [[ -n "$control" ]] || continue
        safe_playback_control "$control" || continue
        details="$(amixer -c "$card" sget "$control" 2>/dev/null || true)"
        printf '%s\n' "$details" | grep -Eq 'Capabilities:.*[[:space:]]pvolume([[:space:]]|$)' || continue
        printf '%s\n' "$details" | grep -Eq 'Playback.*\[[+-]?[0-9]+([.][0-9]+)?dB\]' || continue

        if amixer -q -c "$card" sset "$control" 0dB >/dev/null 2>&1; then
            after="$(amixer -c "$card" sget "$control" 2>/dev/null || true)"
            if printf '%s\n' "$after" | grep -Eq 'Playback.*\[[+-]?0+([.]0+)?dB\]'; then
                echo "ALSA card $card: '$control' встановлено на hardware unity 0 dB"
                applied=1
            else
                echo "ALSA card $card: '$control' не має точного 0 dB; залишено найближче значення драйвера" >&2
            fi
        fi
    done < <(
        amixer -c "$card" scontrols 2>/dev/null |
            sed -n "s/^Simple mixer control '\(.*\)',[0-9][0-9]*$/\1/p"
    )

    if ((applied == 0)); then
        echo "ALSA card $card: безпечного playback-контролу з 0 dB не знайдено; hardware mixer не вгадується"
    fi
}

prepare_physical_sink() {
    local physical="$1"
    if ! [[ "$OUTPUT_VOLUME_PERCENT" =~ ^[0-9]+$ ]] || ((10#$OUTPUT_VOLUME_PERCENT > 100)); then
        echo "OUTPUT_VOLUME_PERCENT має бути цілим числом від 0 до 100" >&2
        return 1
    fi
    prepare_hardware_mixer "$physical"
    pactl set-sink-volume "$physical" "${OUTPUT_VOLUME_PERCENT}%"
    pactl set-sink-mute "$physical" 0
}

load_loopback() {
    local source="$1" physical="$2"
    pactl load-module module-loopback \
        source="$source.monitor" sink="$physical" \
        latency_msec="$LOOPBACK_LATENCY_MSEC" \
        source_dont_move=true sink_dont_move=true
}

start_buses() {
    unload_saved_modules
    local physical music_bus alert_bus music_loop alert_loop
    if ! [[ "$SAMPLE_RATE" =~ ^[0-9]+$ ]] || ((10#$SAMPLE_RATE < 8000 || 10#$SAMPLE_RATE > 384000)); then
        echo "SAMPLE_RATE має бути цілим числом від 8000 до 384000" >&2
        return 1
    fi
    physical="$(wait_for_physical_sink)"
    prepare_physical_sink "$physical"

    music_bus="$(pactl load-module module-null-sink \
        sink_name="$MUSIC_SINK" \
        sink_properties="device.description=proaudio_player_music_Bus" \
        rate="$SAMPLE_RATE" channels=2)"
    alert_bus="$(pactl load-module module-null-sink \
        sink_name="$ALERT_SINK" \
        sink_properties="device.description=proaudio_player_alert_Bus" \
        rate="$SAMPLE_RATE" channels=2)"
    music_loop="$(load_loopback "$MUSIC_SINK" "$physical")"
    alert_loop="$(load_loopback "$ALERT_SINK" "$physical")"
    write_state "$physical" "$music_bus" "$alert_bus" "$music_loop" "$alert_loop"

    pactl set-default-sink "$MUSIC_SINK"
    pactl set-sink-volume "$MUSIC_SINK" 100%
    pactl set-sink-volume "$ALERT_SINK" 100%
    pactl set-sink-mute "$MUSIC_SINK" 0
    pactl set-sink-mute "$ALERT_SINK" 0
    echo "Музична й службова шини підключені до $physical; програмний вихід ${OUTPUT_VOLUME_PERCENT}%"
}

switch_output() {
    local physical music_bus alert_bus old_music_loop old_alert_loop new_music_loop new_alert_loop
    physical="$(wait_for_physical_sink)"
    prepare_physical_sink "$physical"

    music_bus="$(state_value MUSIC_BUS_MODULE)"
    alert_bus="$(state_value ALERT_BUS_MODULE)"
    old_music_loop="$(state_value MUSIC_LOOP_MODULE)"
    old_alert_loop="$(state_value ALERT_LOOP_MODULE)"

    if [[ -z "$music_bus" || -z "$alert_bus" ]] \
        || ! pactl get-sink-volume "$MUSIC_SINK" >/dev/null 2>&1 \
        || ! pactl get-sink-volume "$ALERT_SINK" >/dev/null 2>&1; then
        echo "Стан постійних шин відсутній; виконується повне відновлення" >&2
        start_buses
        return
    fi

    new_music_loop="$(load_loopback "$MUSIC_SINK" "$physical")"
    if ! new_alert_loop="$(load_loopback "$ALERT_SINK" "$physical")"; then
        pactl unload-module "$new_music_loop" >/dev/null 2>&1 || true
        return 1
    fi

    [[ "$old_music_loop" =~ ^[0-9]+$ ]] && pactl unload-module "$old_music_loop" >/dev/null 2>&1 || true
    [[ "$old_alert_loop" =~ ^[0-9]+$ ]] && pactl unload-module "$old_alert_loop" >/dev/null 2>&1 || true
    write_state "$physical" "$music_bus" "$alert_bus" "$new_music_loop" "$new_alert_loop"
    echo "Вихід постійних шин безрозривно перемкнено на $physical"
}

stop_buses() {
    local physical
    physical="$(state_value PHYSICAL)"
    unload_saved_modules
    if [[ -n "$physical" ]]; then
        pactl set-default-sink "$physical" >/dev/null 2>&1 || true
    fi
}

case "${1:-}" in
    start) start_buses ;;
    switch) switch_output ;;
    stop) stop_buses ;;
    restart) stop_buses; start_buses ;;
    *) echo "Використання: $0 {start|switch|stop|restart}" >&2; exit 2 ;;
esac
