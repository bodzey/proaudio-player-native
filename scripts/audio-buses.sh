#!/usr/bin/env bash
set -euo pipefail

MUSIC_SINK="${MUSIC_SINK:-proaudio_player_music}"
ALERT_SINK="${ALERT_SINK:-proaudio_player_alert}"
MASTER_SINK="${MASTER_SINK:-proaudio_player_master}"
PHYSICAL_SINK="${PHYSICAL_SINK:-AUTO}"
SAMPLE_RATE="${SAMPLE_RATE:-48000}"
AUDIO_CHANNELS="${AUDIO_CHANNELS:-2}"
LOOPBACK_LATENCY_MSEC="${LOOPBACK_LATENCY_MSEC:-100}"
OUTPUT_VOLUME_PERCENT="${OUTPUT_VOLUME_PERCENT:-100}"
HARDWARE_MIXER_MODE="${HARDWARE_MIXER_MODE:-unity}"
SINK_WAIT_SECONDS="${SINK_WAIT_SECONDS:-30}"
STATE_FILE="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is not set}/proaudio-player-bus-modules"

physical_candidates() {
    pactl list short sinks |
        awk -v music="$MUSIC_SINK" -v alert="$ALERT_SINK" -v master="$MASTER_SINK" '
            $2 != music && $2 != alert && $2 != master && $2 != "auto_null" &&
            $2 !~ /^proaudio_player_/ {
                priority = ($NF == "RUNNING" ? 0 : 1)
                print priority "\t" $2
            }' |
        sort -k1,1n -k2,2 |
        cut -f2-
}

find_physical_sink() {
    if [[ "$PHYSICAL_SINK" != "AUTO" ]]; then
        if pactl get-sink-volume "$PHYSICAL_SINK" >/dev/null 2>&1; then
            printf '%s\n' "$PHYSICAL_SINK"
            return 0
        fi
        return 1
    fi

    # AUTO is intentionally bus-neutral. Prefer an already RUNNING physical sink;
    # otherwise use a deterministic name-sorted fallback. Device-specific priority
    # belongs to a firmware profile, never to the generic player core.
    physical_candidates | head -n 1
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
    local physical="$1" master_bus="$2" music_bus="$3" alert_bus="$4" music_loop="$5" alert_loop="$6"
    local temporary="${STATE_FILE}.tmp"
    {
        printf 'PHYSICAL=%s\n' "$physical"
        printf 'MASTER_SINK=%s\n' "$MASTER_SINK"
        printf 'MASTER_BUS_MODULE=%s\n' "$master_bus"
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

playback_db_values() {
    sed -n 's/.*Playback.*\[\([+-]\{0,1\}[0-9][0-9]*\(\.[0-9][0-9]*\)\{0,1\}\)dB\].*/\1/p'
}

playback_raw_values() {
    sed -n 's/.*Playback \(-\{0,1\}[0-9][0-9]*\) \[[0-9][0-9]*%\].*/\1/p' | paste -sd, -
}

max_db_value() {
    awk 'NR == 1 { max=$1 } $1 > max { max=$1 } END { if (NR) print max }'
}

prepare_hardware_mixer() {
    local physical="$1"
    [[ "$HARDWARE_MIXER_MODE" == "off" ]] && return 0
    if [[ "$HARDWARE_MIXER_MODE" != "unity" ]]; then
        echo "HARDWARE_MIXER_MODE має бути 'unity' або 'off'" >&2
        return 1
    fi
    command -v amixer >/dev/null 2>&1 || return 0

    local card control details after max_db target original_raw applied=0
    card="$(alsa_card_for_sink "$physical" || true)"
    [[ "$card" =~ ^[0-9]+$ ]] || return 0

    while IFS= read -r control; do
        [[ -n "$control" ]] || continue
        safe_playback_control "$control" || continue
        details="$(amixer -c "$card" sget "$control" 2>/dev/null || true)"
        printf '%s\n' "$details" | grep -Eq 'Capabilities:.*[[:space:]]pvolume([[:space:]]|$)' || continue
        printf '%s\n' "$details" | playback_db_values | grep -q . || continue
        original_raw="$(printf '%s\n' "$details" | playback_raw_values)"

        # The physical sink is muted by prepare_physical_sink while this probe runs.
        # Ask ALSA for 0 dB, then verify the actual quantized hardware value. If a
        # coarse control rounded above unity, request a negative value and verify
        # again. Any failed probe is rolled back to the exact raw value we found.
        if ! amixer -q -c "$card" sset "$control" 0dB >/dev/null 2>&1; then
            continue
        fi
        after="$(amixer -c "$card" sget "$control" 2>/dev/null || true)"
        max_db="$(printf '%s\n' "$after" | playback_db_values | max_db_value)"
        [[ -n "$max_db" ]] || {
            if [[ -n "$original_raw" ]]; then
                amixer -q -c "$card" sset "$control" "$original_raw" >/dev/null 2>&1 || true
            fi
            continue
        }

        if awk -v value="$max_db" 'BEGIN { exit !(value > 0.0) }'; then
            target="$(awk -v value="$max_db" 'BEGIN { printf "%.2fdB", -(value + 0.10) }')"
            amixer -q -c "$card" sset "$control" "$target" >/dev/null 2>&1 || true
            after="$(amixer -c "$card" sget "$control" 2>/dev/null || true)"
            max_db="$(printf '%s\n' "$after" | playback_db_values | max_db_value)"
        fi

        if [[ -n "$max_db" ]] && awk -v value="$max_db" 'BEGIN { exit !(value <= 0.0001) }'; then
            echo "ALSA card $card: '$control' hardware level = ${max_db} dB (safe unity ceiling)"
            applied=1
            continue
        fi

        if [[ -n "$original_raw" ]] && amixer -q -c "$card" sset "$control" "$original_raw" >/dev/null 2>&1; then
            echo "ALSA card $card: '$control' не має надійного <= 0 dB unity; початковий raw-рівень відновлено" >&2
        else
            echo "ALSA card $card: '$control' не вдалося безпечно нормалізувати або відновити" >&2
            return 1
        fi
    done < <(
        amixer -c "$card" scontrols 2>/dev/null |
            sed -n "s/^Simple mixer control '\(.*\)',[0-9][0-9]*$/\1/p"
    )

    if ((applied == 0)); then
        echo "ALSA card $card: безпечного playback-контролу з dB-шкалою не знайдено; hardware mixer не вгадується"
    fi
}

prepare_physical_sink() {
    local physical="$1"
    if ! [[ "$OUTPUT_VOLUME_PERCENT" =~ ^[0-9]+$ ]] || ((10#$OUTPUT_VOLUME_PERCENT > 100)); then
        echo "OUTPUT_VOLUME_PERCENT має бути цілим числом від 0 до 100" >&2
        return 1
    fi

    # Mute while hardware gain is normalized so a coarse ALSA control can never
    # produce an audible positive-gain transient during probing.
    pactl set-sink-mute "$physical" 1
    prepare_hardware_mixer "$physical"
    pactl set-sink-volume "$physical" "${OUTPUT_VOLUME_PERCENT}%"
    pactl set-sink-mute "$physical" 0
}

load_loopback() {
    local source="$1" target="$2"
    pactl load-module module-loopback \
        source="$source.monitor" sink="$target" \
        latency_msec="$LOOPBACK_LATENCY_MSEC" \
        source_dont_move=true sink_dont_move=true
}

validate_audio_bus_config() {
    if ! [[ "$SAMPLE_RATE" =~ ^[0-9]+$ ]] || ((10#$SAMPLE_RATE < 8000 || 10#$SAMPLE_RATE > 384000)); then
        echo "SAMPLE_RATE має бути цілим числом від 8000 до 384000" >&2
        return 1
    fi
    if ! [[ "$AUDIO_CHANNELS" =~ ^[0-9]+$ ]] || ((10#$AUDIO_CHANNELS < 1 || 10#$AUDIO_CHANNELS > 8)); then
        echo "AUDIO_CHANNELS має бути цілим числом від 1 до 8" >&2
        return 1
    fi
}

start_buses() {
    unload_saved_modules
    validate_audio_bus_config

    local physical master_bus music_bus alert_bus music_loop alert_loop
    physical="$(wait_for_physical_sink)"
    prepare_physical_sink "$physical"

    master_bus="$(pactl load-module module-null-sink \
        sink_name="$MASTER_SINK" \
        sink_properties="device.description=ProAudio_Player_Final_Mix" \
        rate="$SAMPLE_RATE" channels="$AUDIO_CHANNELS")"
    music_bus="$(pactl load-module module-null-sink \
        sink_name="$MUSIC_SINK" \
        sink_properties="device.description=ProAudio_Player_Music_Bus" \
        rate="$SAMPLE_RATE" channels="$AUDIO_CHANNELS")"
    alert_bus="$(pactl load-module module-null-sink \
        sink_name="$ALERT_SINK" \
        sink_properties="device.description=ProAudio_Player_Alert_Bus" \
        rate="$SAMPLE_RATE" channels="$AUDIO_CHANNELS")"

    # MUSIC and ALERT are mixed in float by PipeWire/Pulse into one final bus.
    # The safety limiter is the only path from MASTER.monitor to the physical sink.
    music_loop="$(load_loopback "$MUSIC_SINK" "$MASTER_SINK")"
    alert_loop="$(load_loopback "$ALERT_SINK" "$MASTER_SINK")"
    write_state "$physical" "$master_bus" "$music_bus" "$alert_bus" "$music_loop" "$alert_loop"

    pactl set-default-sink "$MUSIC_SINK"
    pactl set-sink-volume "$MUSIC_SINK" 100%
    pactl set-sink-volume "$ALERT_SINK" 100%
    pactl set-sink-volume "$MASTER_SINK" 100%
    pactl set-sink-mute "$MUSIC_SINK" 0
    pactl set-sink-mute "$ALERT_SINK" 0
    pactl set-sink-mute "$MASTER_SINK" 0
    echo "MUSIC + ALERT -> $MASTER_SINK -> safety limiter -> $physical"
}

switch_output() {
    validate_audio_bus_config
    local physical master_bus music_bus alert_bus music_loop alert_loop
    physical="$(wait_for_physical_sink)"

    master_bus="$(state_value MASTER_BUS_MODULE)"
    music_bus="$(state_value MUSIC_BUS_MODULE)"
    alert_bus="$(state_value ALERT_BUS_MODULE)"
    music_loop="$(state_value MUSIC_LOOP_MODULE)"
    alert_loop="$(state_value ALERT_LOOP_MODULE)"

    if [[ -z "$master_bus" || -z "$music_bus" || -z "$alert_bus" ]] \
        || ! pactl get-sink-volume "$MASTER_SINK" >/dev/null 2>&1 \
        || ! pactl get-sink-volume "$MUSIC_SINK" >/dev/null 2>&1 \
        || ! pactl get-sink-volume "$ALERT_SINK" >/dev/null 2>&1; then
        echo "Стан постійних шин відсутній; виконується повне відновлення" >&2
        start_buses
        return
    fi

    prepare_physical_sink "$physical"
    # Atomic state replacement is watched by systemd; the limiter restarts against
    # the new physical sink while the logical MUSIC/ALERT topology remains intact.
    write_state "$physical" "$master_bus" "$music_bus" "$alert_bus" "$music_loop" "$alert_loop"
    echo "Фінальний вихід перемкнено на $physical"
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
