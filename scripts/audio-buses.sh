#!/usr/bin/env bash
set -euo pipefail

MUSIC_SINK="${MUSIC_SINK:-proaudio_player_music}"
ALERT_SINK="${ALERT_SINK:-proaudio_player_alert}"
MASTER_SINK="${MASTER_SINK:-proaudio_player_master}"
PARKING_SINK="${PARKING_SINK:-proaudio_player_parking}"
PHYSICAL_SINK="${PHYSICAL_SINK:-AUTO}"
SAMPLE_RATE_MODE="${SAMPLE_RATE_MODE:-fixed}"
SAMPLE_RATE="${SAMPLE_RATE:-48000}"
ALLOWED_SAMPLE_RATES="${ALLOWED_SAMPLE_RATES:-$SAMPLE_RATE}"
AUDIO_CHANNELS="${AUDIO_CHANNELS:-2}"
INTERNAL_SAMPLE_FORMAT="${INTERNAL_SAMPLE_FORMAT:-float32le}"
LOOPBACK_LATENCY_MSEC="${LOOPBACK_LATENCY_MSEC:-100}"
OUTPUT_VOLUME_PERCENT="${OUTPUT_VOLUME_PERCENT:-100}"
HARDWARE_MIXER_MODE="${HARDWARE_MIXER_MODE:-off}"
GRAPH_UNITY_DB="0.0"
GRAPH_UNITY_RAW="65536"
STATE_FILE="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is not set}/proaudio-player-bus-modules"
LOCK_DIR="${XDG_RUNTIME_DIR}/proaudio-player-audio-routing.lock"
BUILD_MODULES=()

acquire_lock() {
    local deadline=$((SECONDS + 15))
    while ! mkdir "$LOCK_DIR" 2>/dev/null; do
        local owner=""
        owner="$(cat "$LOCK_DIR/pid" 2>/dev/null || true)"
        if [[ "$owner" =~ ^[0-9]+$ ]] && ! kill -0 "$owner" 2>/dev/null; then
            rm -f -- "$LOCK_DIR/pid"
            rmdir "$LOCK_DIR" 2>/dev/null || true
            continue
        fi
        if ((SECONDS >= deadline)); then
            echo "Не вдалося отримати блокування маршрутизації аудіо" >&2
            return 1
        fi
        sleep 0.1
    done
    printf '%s\n' "$$" >"$LOCK_DIR/pid"
    trap 'rm -f -- "$LOCK_DIR/pid"; rmdir "$LOCK_DIR" >/dev/null 2>&1 || true' EXIT INT TERM
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

state_value() {
    local wanted="$1"
    [[ -f "$STATE_FILE" ]] || return 0
    awk -F= -v wanted="$wanted" '$1 == wanted { print substr($0, index($0,"=")+1); exit }' "$STATE_FILE"
}

write_state() {
    local physical="$1" output_target="$2" parking_bus="$3" master_bus="$4"
    local music_bus="$5" alert_bus="$6" music_loop="$7" alert_loop="$8" output_loop="$9"
    local temporary="${STATE_FILE}.tmp"
    {
        printf 'PHYSICAL=%s\n' "$physical"
        printf 'OUTPUT_TARGET=%s\n' "$output_target"
        printf 'PARKING_BUS_MODULE=%s\n' "$parking_bus"
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

load_module_into() {
    local destination="$1"
    shift
    local module
    module="$(pactl load-module "$@")" || return 1
    [[ "$module" =~ ^[0-9]+$ ]] || return 1
    printf -v "$destination" '%s' "$module"
    BUILD_MODULES+=("$module")
}
load_bus_sink_into() {
    local destination="$1" sink_name="$2" description="$3"
    local args=(
        module-null-sink
        "sink_name=$sink_name"
        "sink_properties=device.description=$description monitor.channel-volumes=true"
        "format=$INTERNAL_SAMPLE_FORMAT"
        "channels=$AUDIO_CHANNELS"
    )
    if [[ "$SAMPLE_RATE_MODE" == "fixed" ]]; then
        args+=("rate=$SAMPLE_RATE")
    fi
    load_module_into "$destination" "${args[@]}"
}

pipewire_allowed_rates_json() {
    local rate result="["
    IFS=',' read -ra rates <<<"$ALLOWED_SAMPLE_RATES"
    for rate in "${rates[@]}"; do
        rate="${rate//[[:space:]]/}"
        [[ -n "$rate" ]] || continue
        result+=" $rate"
    done
    printf '%s ]\n' "$result"
}

configure_pipewire_rate_policy() {
    [[ "$SAMPLE_RATE_MODE" == "adaptive" ]] || return 0

    if ! command -v pw-metadata >/dev/null 2>&1; then
        echo "SAMPLE_RATE_MODE=adaptive потребує pw-metadata" >&2
        return 1
    fi

    local allowed
    allowed="$(pipewire_allowed_rates_json)"
    if ! pw-metadata -n settings 0 clock.allowed-rates "$allowed" >/dev/null 2>&1; then
        echo "Не вдалося застосувати PipeWire clock.allowed-rates=$allowed" >&2
        return 1
    fi
    if ! pw-metadata -n settings 0 clock.force-rate 0 >/dev/null 2>&1; then
        echo "Не вдалося дозволити автоматичне перемикання PipeWire graph rate" >&2
        return 1
    fi

    echo "PipeWire adaptive rate policy: fallback ${SAMPLE_RATE} Hz, allowed $allowed"
}


cleanup_build_modules() {
    local index
    for ((index=${#BUILD_MODULES[@]} - 1; index >= 0; index--)); do
        pactl unload-module "${BUILD_MODULES[index]}" >/dev/null 2>&1 || true
    done
    BUILD_MODULES=()
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

physical_sink_software_is_unity() {
    local physical="$1"
    [[ "$(pactl get-sink-mute "$physical" 2>/dev/null | awk '{print $2}')" == "no" ]] || return 1
    pactl get-sink-volume "$physical" 2>/dev/null |
        tr '/,' '\n' |
        awk '
            BEGIN { valid=1 }
            /^[[:space:]]*[0-9]+%[[:space:]]*$/ {
                value=$1
                found=1
                if (value != "100%") valid=0
            }
            END { exit !(found && valid) }'
}

prepare_physical_sink() {
    local physical="$1"
    if [[ "$OUTPUT_VOLUME_PERCENT" != "100" ]]; then
        echo "OUTPUT_VOLUME_PERCENT має бути 100: фізичний software sink є фіксованим unity stage" >&2
        return 1
    fi

    # Generic runtime never toggles the physical sink merely to prepare a route:
    # hardware/DAC mute or mixer writes can create analogue transients even when
    # MASTER is digitally muted. Device-specific hardware normalization is an
    # explicit firmware opt-in through HARDWARE_MIXER_MODE=unity.
    if [[ "$HARDWARE_MIXER_MODE" == "unity" ]]; then
        prepare_hardware_mixer "$physical"
    elif [[ "$HARDWARE_MIXER_MODE" != "off" ]]; then
        echo "HARDWARE_MIXER_MODE має бути 'unity' або 'off'" >&2
        return 1
    fi

    if ! physical_sink_software_is_unity "$physical"; then
        pactl set-sink-volume "$physical" 100%
        pactl set-sink-mute "$physical" 0
    fi
}

load_loopback_into() {
    local destination="$1" source="$2" target="$3" stream_name="$4"
    load_module_into "$destination" module-loopback \
        source="$source.monitor" sink="$target" \
        latency_msec="$LOOPBACK_LATENCY_MSEC" \
        source_output_properties="node.passive=true resample.quality=10" \
        sink_input_properties="media.name=$stream_name node.passive=true resample.quality=10" \
        source_dont_move=true sink_dont_move=true
}

load_final_loopback_into() {
    local destination="$1" source="$2" target="$3"
    load_module_into "$destination" module-loopback \
        source="$source.monitor" sink="$target" \
        latency_msec="$LOOPBACK_LATENCY_MSEC" \
        source_output_properties="node.passive=true resample.quality=10" \
        sink_input_properties="media.name=proaudio-player-final-output node.passive=true resample.quality=10" \
        source_dont_move=true
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

set_loopback_gain_db() {
    local module="$1" db="$2" label="$3"
    local input=""
    if [[ "$db" != "$GRAPH_UNITY_DB" ]]; then
        echo "$label: дозволено лише фіксований unity gain ${GRAPH_UNITY_DB} dB" >&2
        return 1
    fi
    input="$(sink_input_for_module "$module" || true)"
    if ! [[ "$input" =~ ^[0-9]+$ ]]; then
        sleep 0.1
        input="$(sink_input_for_module "$module" || true)"
    fi
    if [[ "$input" =~ ^[0-9]+$ ]]; then
        # PA_VOLUME_NORM is an exact absolute unity value and needs no floating
        # point math. This stays compatible with BusyBox awk builds without math.
        pactl set-sink-input-volume "$input" "$GRAPH_UNITY_RAW"
        echo "$label gain = ${db} dB (absolute raw $GRAPH_UNITY_RAW, sink-input $input)"
        return 0
    fi
    echo "Не вдалося знайти sink-input для $label module $module" >&2
    return 1
}

validate_audio_bus_config() {
    if [[ "$SAMPLE_RATE_MODE" != "fixed" && "$SAMPLE_RATE_MODE" != "adaptive" ]]; then
        echo "SAMPLE_RATE_MODE має бути fixed або adaptive" >&2
        return 1
    fi
    if ! [[ "$SAMPLE_RATE" =~ ^[0-9]+$ ]] || ((10#$SAMPLE_RATE < 8000 || 10#$SAMPLE_RATE > 384000)); then
        echo "SAMPLE_RATE має бути цілим числом від 8000 до 384000" >&2
        return 1
    fi

    local rate found=0
    IFS=',' read -ra rates <<<"$ALLOWED_SAMPLE_RATES"
    for rate in "${rates[@]}"; do
        rate="${rate//[[:space:]]/}"
        if ! [[ "$rate" =~ ^[0-9]+$ ]] || ((10#$rate < 8000 || 10#$rate > 384000)); then
            echo "ALLOWED_SAMPLE_RATES містить некоректну частоту: $rate" >&2
            return 1
        fi
        [[ "$rate" == "$SAMPLE_RATE" ]] && found=1
    done
    if [[ "$SAMPLE_RATE_MODE" == "adaptive" && "$found" != "1" ]]; then
        echo "SAMPLE_RATE має входити до ALLOWED_SAMPLE_RATES в adaptive mode" >&2
        return 1
    fi

    if ! [[ "$AUDIO_CHANNELS" =~ ^[0-9]+$ ]] || ((10#$AUDIO_CHANNELS < 1 || 10#$AUDIO_CHANNELS > 8)); then
        echo "AUDIO_CHANNELS має бути цілим числом від 1 до 8" >&2
        return 1
    fi
    if [[ "$AUDIO_CHANNELS" != "2" ]]; then
        echo "Поточний mixed graph підтримує рівно 2 канали" >&2
        return 1
    fi
    if [[ "$INTERNAL_SAMPLE_FORMAT" != "float32le" ]]; then
        echo "INTERNAL_SAMPLE_FORMAT має бути float32le для high-precision mixed graph" >&2
        return 1
    fi
}

start_buses() {
    unload_saved_modules
    validate_audio_bus_config
    require_pulse_server
    configure_pipewire_rate_policy

    local physical output_target parking_bus master_bus music_bus alert_bus
    local music_loop alert_loop output_loop
    physical="$(find_physical_sink || true)"
    output_target="${physical:-$PARKING_SINK}"
    if [[ -n "$physical" ]] && ! prepare_physical_sink "$physical"; then
        return 1
    fi

    BUILD_MODULES=()
    if ! load_bus_sink_into parking_bus "$PARKING_SINK" "ProAudio_Player_Parking_Output" \
        || ! load_bus_sink_into master_bus "$MASTER_SINK" "ProAudio_Player_Final_Mix" \
        || ! load_bus_sink_into music_bus "$MUSIC_SINK" "ProAudio_Player_Music_Bus" \
        || ! load_bus_sink_into alert_bus "$ALERT_SINK" "ProAudio_Player_Alert_Bus" \
        || ! load_loopback_into music_loop "$MUSIC_SINK" "$MASTER_SINK" \
            "proaudio-player-music-to-master" \
        || ! load_loopback_into alert_loop "$ALERT_SINK" "$MASTER_SINK" \
            "proaudio-player-alert-to-master" \
        || ! load_final_loopback_into output_loop "$MASTER_SINK" "$output_target"; then
        echo "Не вдалося створити повний аудіограф; часткові модулі видаляються" >&2
        cleanup_build_modules
        return 1
    fi

    if ! set_loopback_gain_db "$music_loop" "$GRAPH_UNITY_DB" "MUSIC->MASTER" \
        || ! set_loopback_gain_db "$alert_loop" "$GRAPH_UNITY_DB" "ALERT->MASTER"; then
        cleanup_build_modules
        return 1
    fi
    if ! set_loopback_gain_db "$output_loop" "$GRAPH_UNITY_DB" "MASTER->OUTPUT"; then
        cleanup_build_modules
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
        cleanup_build_modules
        return 1
    fi

    if ! write_state "$physical" "$output_target" "$parking_bus" "$master_bus" \
        "$music_bus" "$alert_bus" "$music_loop" "$alert_loop" "$output_loop"; then
        echo "Не вдалося зафіксувати стан аудіографа; створені модулі видаляються" >&2
        cleanup_build_modules
        return 1
    fi
    BUILD_MODULES=()

    echo "MUSIC + ALERT -> $MASTER_SINK -> $output_target (PipeWire/Pulse graph)"
}

switch_output() {
    validate_audio_bus_config
    require_pulse_server

    local physical output_target old_physical old_output_target output_loop output_input
    local parking_bus master_bus music_bus alert_bus music_loop alert_loop
    physical="$(find_physical_sink || true)"
    output_target="${physical:-$PARKING_SINK}"
    old_physical="$(state_value PHYSICAL)"
    old_output_target="$(state_value OUTPUT_TARGET)"
    old_output_target="${old_output_target:-${old_physical:-$PARKING_SINK}}"
    output_loop="$(state_value OUTPUT_LOOP_MODULE)"
    parking_bus="$(state_value PARKING_BUS_MODULE)"
    master_bus="$(state_value MASTER_BUS_MODULE)"
    music_bus="$(state_value MUSIC_BUS_MODULE)"
    alert_bus="$(state_value ALERT_BUS_MODULE)"
    music_loop="$(state_value MUSIC_LOOP_MODULE)"
    alert_loop="$(state_value ALERT_LOOP_MODULE)"

    if [[ -z "$parking_bus" || -z "$master_bus" || -z "$music_bus" || -z "$alert_bus" \
        || -z "$music_loop" || -z "$alert_loop" || ! "$output_loop" =~ ^[0-9]+$ ]] \
        || ! pactl get-sink-volume "$PARKING_SINK" >/dev/null 2>&1 \
        || ! pactl get-sink-volume "$MASTER_SINK" >/dev/null 2>&1 \
        || ! pactl get-sink-volume "$MUSIC_SINK" >/dev/null 2>&1 \
        || ! pactl get-sink-volume "$ALERT_SINK" >/dev/null 2>&1; then
        echo "Стан постійних шин відсутній; виконується повне відновлення" >&2
        start_buses
        return
    fi

    if [[ -n "$physical" ]] && ! prepare_physical_sink "$physical"; then
        echo "Не вдалося підготувати фізичний вихід '$physical'; попередній маршрут залишено" >&2
        return 1
    fi

    if ! set_loopback_gain_db "$music_loop" "$GRAPH_UNITY_DB" "MUSIC->MASTER" \
        || ! set_loopback_gain_db "$alert_loop" "$GRAPH_UNITY_DB" "ALERT->MASTER" \
        || ! set_loopback_gain_db "$output_loop" "$GRAPH_UNITY_DB" "MASTER->OUTPUT"; then
        echo "Не вдалося підтвердити unity gain аудіографа; попередній маршрут залишено" >&2
        return 1
    fi

    if [[ "$old_output_target" == "$output_target" ]]; then
        return
    fi

    output_input="$(sink_input_for_module "$output_loop" || true)"
    if ! [[ "$output_input" =~ ^[0-9]+$ ]]; then
        echo "Не вдалося знайти постійний MASTER->OUTPUT sink-input; перемикання не виконано" >&2
        return 1
    fi

    # Keep one permanent final playback stream and move it between sinks in-place.
    # Recreating module-loopback on every UI selection needlessly destroys/creates
    # a physical playback stream and can produce DAC/codec activation transients.
    if ! pactl move-sink-input "$output_input" "$output_target"; then
        echo "Не вдалося перенести MASTER->OUTPUT на '$output_target'; попередній маршрут залишено" >&2
        return 1
    fi

    if ! write_state "$physical" "$output_target" "$parking_bus" "$master_bus" \
        "$music_bus" "$alert_bus" "$music_loop" "$alert_loop" "$output_loop"; then
        pactl move-sink-input "$output_input" "$old_output_target" >/dev/null 2>&1 || true
        echo "Не вдалося зафіксувати новий маршрут; playback stream повернено на '$old_output_target'" >&2
        return 1
    fi

    echo "Фінальний playback stream перенесено $old_output_target -> $output_target"
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
