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
OUTPUT_HEADROOM_DB="${OUTPUT_HEADROOM_DB:--1.0}"
ALERT_MIX_GAIN_DB="${ALERT_MIX_GAIN_DB:-0.0}"
HARDWARE_MIXER_MODE="${HARDWARE_MIXER_MODE:-unity}"
SINK_WAIT_SECONDS="${SINK_WAIT_SECONDS:-30}"
STATE_FILE="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is not set}/proaudio-player-bus-modules"
LOCK_DIR="${XDG_RUNTIME_DIR}/proaudio-player-audio-routing.lock"

acquire_lock() {
    local deadline=$((SECONDS + 15))
    while ! mkdir "$LOCK_DIR" 2>/dev/null; do
        if ((SECONDS >= deadline)); then
            echo "Не вдалося отримати блокування маршрутизації аудіо" >&2
            return 1
        fi
        sleep 0.1
    done
    trap 'rmdir "$LOCK_DIR" >/dev/null 2>&1 || true' EXIT INT TERM
}

require_pulse_server() {
    if ! pactl info >/dev/null 2>&1; then
        echo "PipeWire-Pulse недоступний; запуск аудіографа припинено" >&2
        return 1
    fi
}

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
    physical_candidates | head -n 1
}

wait_for_physical_sink() {
    require_pulse_server
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
    local physical="$1" master_bus="$2" music_bus="$3" alert_bus="$4"
    local music_loop="$5" alert_loop="$6" output_loop="$7"
    local temporary="${STATE_FILE}.tmp"
    {
        printf 'PHYSICAL=%s\n' "$physical"
        printf 'MASTER_SINK=%s\n' "$MASTER_SINK"
        printf 'MASTER_BUS_MODULE=%s\n' "$master_bus"
        printf 'MUSIC_BUS_MODULE=%s\n' "$music_bus"
        printf 'ALERT_BUS_MODULE=%s\n' "$alert_bus"
        printf 'MUSIC_LOOP_MODULE=%s\n' "$music_loop"
        printf 'ALERT_LOOP_MODULE=%s\n' "$alert_loop"
        printf 'OUTPUT_LOOP_MODULE=%s\n' "$output_loop"
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
            if ($3 ~ /^[0-9]+$/) { print $3; exit }
        }'
}

safe_playback_control() {
    local name="${1,,}"
    case "$name" in
        *capture*|*mic*|*boost*|*gain*|*input*|*adc*|*loopback*|*monitor*|*tone*|*bass*|*treble*) return 1 ;;
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

playback_has_zero_percent() {
    grep -Eq 'Playback[[:space:]]+-?[0-9]+[[:space:]]+\[0%\]'
}

reapply_playback_channels() {
    local card="$1" control="$2" details="$3"
    local raw_values
    raw_values="$(printf '%s\n' "$details" | playback_raw_values)"
    [[ -n "$raw_values" ]] || return 1
    amixer -q -c "$card" sset "$control" "$raw_values" >/dev/null 2>&1
}

restore_hardware_raw() {
    local card="$1" control="$2" raw="$3"
    [[ -n "$raw" ]] && amixer -q -c "$card" sset "$control" "$raw" >/dev/null 2>&1
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

        if ! amixer -q -c "$card" sset "$control" 0dB >/dev/null 2>&1; then
            continue
        fi
        after="$(amixer -c "$card" sget "$control" 2>/dev/null || true)"

        # Some broken USB descriptors report raw minimum as 0.00 dB. Treating
        # that as unity mutes the device. Never accept a 0 dB probe that lands
        # at 0%; restore the previous raw setting instead.
        if printf '%s\n' "$after" | playback_has_zero_percent; then
            restore_hardware_raw "$card" "$control" "$original_raw" || true
            echo "ALSA card $card: '$control' 0 dB maps to 0%; dB map is untrusted, raw level restored" >&2
            continue
        fi

        max_db="$(printf '%s\n' "$after" | playback_db_values | max_db_value)"
        [[ -n "$max_db" ]] || {
            restore_hardware_raw "$card" "$control" "$original_raw" || true
            continue
        }

        if awk -v value="$max_db" 'BEGIN { exit !(value > 0.0) }'; then
            target="$(awk -v value="$max_db" 'BEGIN { printf "%.2fdB", -(value + 0.10) }')"
            amixer -q -c "$card" sset "$control" "$target" >/dev/null 2>&1 || true
            after="$(amixer -c "$card" sget "$control" 2>/dev/null || true)"
            if printf '%s\n' "$after" | playback_has_zero_percent; then
                restore_hardware_raw "$card" "$control" "$original_raw" || true
                echo "ALSA card $card: '$control' safe dB probe maps to 0%; raw level restored" >&2
                continue
            fi
            max_db="$(printf '%s\n' "$after" | playback_db_values | max_db_value)"
        fi

        if [[ -n "$max_db" ]] && awk -v value="$max_db" 'BEGIN { exit !(value <= 0.0001) }'; then
            if ! reapply_playback_channels "$card" "$control" "$after"; then
                echo "ALSA card $card: '$control' safe level set, but explicit channel initialization failed" >&2
            fi
            after="$(amixer -c "$card" sget "$control" 2>/dev/null || true)"
            max_db="$(printf '%s\n' "$after" | playback_db_values | max_db_value)"
            if [[ -n "$max_db" ]] && ! printf '%s\n' "$after" | playback_has_zero_percent \
                && awk -v value="$max_db" 'BEGIN { exit !(value <= 0.0001) }'; then
                echo "ALSA card $card: '$control' hardware level = ${max_db} dB (safe unity ceiling)"
                applied=1
                continue
            fi
        fi

        if restore_hardware_raw "$card" "$control" "$original_raw"; then
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
        echo "ALSA card $card: безпечного playback-контролу з достовірною dB-шкалою не знайдено; hardware mixer не вгадується"
    fi
}

prepare_physical_sink() {
    local physical="$1"
    if [[ "$OUTPUT_VOLUME_PERCENT" != "100" ]]; then
        echo "OUTPUT_VOLUME_PERCENT має бути 100: фізичний software sink є фіксованим unity stage" >&2
        return 1
    fi
    pactl set-sink-mute "$physical" 1
    prepare_hardware_mixer "$physical"
    pactl set-sink-volume "$physical" 100%
    pactl set-sink-mute "$physical" 0
}

load_loopback() {
    local source="$1" target="$2"
    pactl load-module module-loopback \
        source="$source.monitor" sink="$target" \
        latency_msec="$LOOPBACK_LATENCY_MSEC" \
        source_dont_move=true sink_dont_move=true
}

sink_input_for_module() {
    local module="$1"
    pactl list sink-inputs | awk -v wanted="$module" '
        /^Sink Input #[0-9]+/ {
            input_index=$3
            sub(/^#/, "", input_index)
            next
        }
        /^[[:space:]]*Owner Module:/ && $3 == wanted { print input_index; exit }'
}

pulse_raw_from_db() {
    local db="$1"
    awk -v db="$db" 'BEGIN {
        raw = 65536.0 * exp(log(10.0) * db / 60.0)
        if (raw < 0.0) raw = 0.0
        if (raw > 65536.0) raw = 65536.0
        printf "%.0f\n", raw
    }'
}

set_loopback_gain_db() {
    local module="$1" db="$2" label="$3"
    local input="" raw=""
    input="$(sink_input_for_module "$module" || true)"
    if ! [[ "$input" =~ ^[0-9]+$ ]]; then
        sleep 0.1
        input="$(sink_input_for_module "$module" || true)"
    fi
    if [[ "$input" =~ ^[0-9]+$ ]]; then
        raw="$(pulse_raw_from_db "$db")"
        # Raw Pulse volume is absolute. A signed "-XdB" string is relative in
        # pactl and would accumulate attenuation every time routing is reconciled.
        pactl set-sink-input-volume "$input" "$raw"
        echo "$label gain = ${db} dB (absolute raw $raw, sink-input $input)"
        return 0
    fi
    echo "Не вдалося знайти sink-input для $label module $module" >&2
    return 1
}

validate_attenuation_db() {
    local name="$1" value="$2"
    if ! [[ "$value" =~ ^-?[0-9]+([.][0-9]+)?$ ]] \
        || ! awk -v value="$value" 'BEGIN { exit !(value <= 0.0 && value >= -60.0) }'; then
        echo "$name має бути в межах -60..0 dB" >&2
        return 1
    fi
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
    validate_attenuation_db OUTPUT_HEADROOM_DB "$OUTPUT_HEADROOM_DB"
    validate_attenuation_db ALERT_MIX_GAIN_DB "$ALERT_MIX_GAIN_DB"
}

start_buses() {
    unload_saved_modules
    validate_audio_bus_config
    require_pulse_server

    local physical master_bus music_bus alert_bus music_loop alert_loop output_loop
    physical="$(wait_for_physical_sink)"
    prepare_physical_sink "$physical"

    master_bus="$(pactl load-module module-null-sink \
        sink_name="$MASTER_SINK" \
        sink_properties="device.description=ProAudio_Player_Final_Mix monitor.channel-volumes=true" \
        rate="$SAMPLE_RATE" channels="$AUDIO_CHANNELS")"
    music_bus="$(pactl load-module module-null-sink \
        sink_name="$MUSIC_SINK" \
        sink_properties="device.description=ProAudio_Player_Music_Bus monitor.channel-volumes=true" \
        rate="$SAMPLE_RATE" channels="$AUDIO_CHANNELS")"
    alert_bus="$(pactl load-module module-null-sink \
        sink_name="$ALERT_SINK" \
        sink_properties="device.description=ProAudio_Player_Alert_Bus monitor.channel-volumes=true" \
        rate="$SAMPLE_RATE" channels="$AUDIO_CHANNELS")"

    music_loop="$(load_loopback "$MUSIC_SINK" "$MASTER_SINK")"
    alert_loop="$(load_loopback "$ALERT_SINK" "$MASTER_SINK")"
    output_loop="$(load_loopback "$MASTER_SINK" "$physical")"

    write_state "$physical" "$master_bus" "$music_bus" "$alert_bus" \
        "$music_loop" "$alert_loop" "$output_loop"

    if ! set_loopback_gain_db "$alert_loop" "$ALERT_MIX_GAIN_DB" "ALERT->MASTER"; then
        unload_saved_modules
        return 1
    fi
    if ! set_loopback_gain_db "$output_loop" "$OUTPUT_HEADROOM_DB" "MASTER->OUTPUT"; then
        unload_saved_modules
        return 1
    fi

    if ! pactl set-default-sink "$MUSIC_SINK" \
        || ! pactl set-sink-volume "$MUSIC_SINK" 100% \
        || ! pactl set-sink-volume "$ALERT_SINK" 100% \
        || ! pactl set-sink-volume "$MASTER_SINK" 100% \
        || ! pactl set-sink-mute "$MUSIC_SINK" 0 \
        || ! pactl set-sink-mute "$ALERT_SINK" 0 \
        || ! pactl set-sink-mute "$MASTER_SINK" 0; then
        echo "Не вдалося завершити ініціалізацію аудіографа; створені модулі видаляються" >&2
        unload_saved_modules
        return 1
    fi

    echo "MUSIC + ALERT -> $MASTER_SINK -> $physical (PipeWire/Pulse graph)"
}

switch_output() {
    validate_audio_bus_config
    require_pulse_server

    local physical old_physical old_output_loop new_output_loop
    local master_bus music_bus alert_bus music_loop alert_loop master_mute
    physical="$(wait_for_physical_sink)"
    old_physical="$(state_value PHYSICAL)"
    old_output_loop="$(state_value OUTPUT_LOOP_MODULE)"
    master_bus="$(state_value MASTER_BUS_MODULE)"
    music_bus="$(state_value MUSIC_BUS_MODULE)"
    alert_bus="$(state_value ALERT_BUS_MODULE)"
    music_loop="$(state_value MUSIC_LOOP_MODULE)"
    alert_loop="$(state_value ALERT_LOOP_MODULE)"

    if [[ -z "$master_bus" || -z "$music_bus" || -z "$alert_bus" \
        || -z "$music_loop" || -z "$alert_loop" ]] \
        || ! pactl get-sink-volume "$MASTER_SINK" >/dev/null 2>&1 \
        || ! pactl get-sink-volume "$MUSIC_SINK" >/dev/null 2>&1 \
        || ! pactl get-sink-volume "$ALERT_SINK" >/dev/null 2>&1; then
        echo "Стан постійних шин відсутній; виконується повне відновлення" >&2
        start_buses
        return
    fi

    if [[ "$old_physical" == "$physical" && "$old_output_loop" =~ ^[0-9]+$ ]]; then
        prepare_physical_sink "$physical"
        set_loopback_gain_db "$alert_loop" "$ALERT_MIX_GAIN_DB" "ALERT->MASTER"
        set_loopback_gain_db "$old_output_loop" "$OUTPUT_HEADROOM_DB" "MASTER->OUTPUT"
        return
    fi

    prepare_physical_sink "$physical"
    master_mute="$(pactl get-sink-mute "$MASTER_SINK" 2>/dev/null | awk '{print $2}')"
    pactl set-sink-mute "$MASTER_SINK" 1

    if ! new_output_loop="$(load_loopback "$MASTER_SINK" "$physical")"; then
        [[ "$master_mute" == "yes" ]] || pactl set-sink-mute "$MASTER_SINK" 0 >/dev/null 2>&1 || true
        echo "Не вдалося підключити MASTER до '$physical'; попередній маршрут залишено" >&2
        return 1
    fi

    if ! set_loopback_gain_db "$new_output_loop" "$OUTPUT_HEADROOM_DB" "MASTER->OUTPUT"; then
        pactl unload-module "$new_output_loop" >/dev/null 2>&1 || true
        [[ "$master_mute" == "yes" ]] || pactl set-sink-mute "$MASTER_SINK" 0 >/dev/null 2>&1 || true
        echo "Не вдалося застосувати safety headroom до '$physical'; попередній маршрут залишено" >&2
        return 1
    fi

    if [[ "$old_output_loop" =~ ^[0-9]+$ ]]; then
        pactl unload-module "$old_output_loop" >/dev/null 2>&1 || true
    fi

    write_state "$physical" "$master_bus" "$music_bus" "$alert_bus" \
        "$music_loop" "$alert_loop" "$new_output_loop"
    set_loopback_gain_db "$alert_loop" "$ALERT_MIX_GAIN_DB" "ALERT->MASTER"
    [[ "$master_mute" == "yes" ]] || pactl set-sink-mute "$MASTER_SINK" 0
    echo "Фінальний вихід перемкнено ${old_physical:-<none>} -> $physical"
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
    start|switch|stop|restart) acquire_lock ;;
    *)
        echo "Використання: $0 {start|switch|stop|restart}" >&2
        exit 2
        ;;
esac

case "$1" in
    start) start_buses ;;
    switch) switch_output ;;
    stop) stop_buses ;;
    restart) stop_buses; start_buses ;;
esac
