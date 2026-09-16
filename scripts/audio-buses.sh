#!/usr/bin/env bash
set -euo pipefail

MUSIC_SINK="${MUSIC_SINK:-proaudio_player_music}"
ALERT_SINK="${ALERT_SINK:-proaudio_player_alert}"
MASTER_SINK="${MASTER_SINK:-proaudio_player_master}"
PARKING_SINK="${PARKING_SINK:-proaudio_player_parking}"
PHYSICAL_SINK="${PHYSICAL_SINK:-AUTO}"
SAMPLE_RATE="${SAMPLE_RATE:-48000}"
AUDIO_CHANNELS="${AUDIO_CHANNELS:-2}"
OUTPUT_VOLUME_PERCENT="${OUTPUT_VOLUME_PERCENT:-100}"
HARDWARE_MIXER_MODE="${HARDWARE_MIXER_MODE:-unity}"
STATE_FILE="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is not set}/proaudio-player-bus-modules"
LOCK_DIR="${XDG_RUNTIME_DIR}/proaudio-player-audio-routing.lock"

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

require_pipewire() {
    command -v pw-cli >/dev/null 2>&1 || {
        echo "pw-cli відсутній" >&2
        return 1
    }
    command -v pw-link >/dev/null 2>&1 || {
        echo "pw-link відсутній" >&2
        return 1
    }
    command -v wpctl >/dev/null 2>&1 || {
        echo "wpctl відсутній" >&2
        return 1
    }
    pw-cli info 0 >/dev/null 2>&1 || {
        echo "PipeWire native socket недоступний" >&2
        return 1
    }
}

node_rows() {
    pw-cli ls Node 2>/dev/null | awk '
        function emit() {
            if (id != "" && name != "" && media == "Audio/Sink") {
                print id "\t" name
            }
        }
        /^[[:space:]]*id [0-9]+,/ {
            emit()
            id=$2
            sub(/,$/, "", id)
            name=""
            media=""
            next
        }
        /^[[:space:]]*node.name = / {
            value=$0
            sub(/^[^=]*=[[:space:]]*"/, "", value)
            sub(/"[[:space:]]*$/, "", value)
            name=value
            next
        }
        /^[[:space:]]*media.class = / {
            value=$0
            sub(/^[^=]*=[[:space:]]*"/, "", value)
            sub(/"[[:space:]]*$/, "", value)
            media=value
            next
        }
        END { emit() }
    '
}

node_id() {
    local wanted="$1"
    node_rows | awk -F '\t' -v wanted="$wanted" '$2 == wanted { print $1; exit }'
}

sink_available() {
    [[ -n "$(node_id "$1" || true)" ]]
}

physical_candidates() {
    node_rows |
        awk -F '\t' -v music="$MUSIC_SINK" -v alert="$ALERT_SINK" \
            -v master="$MASTER_SINK" -v parking="$PARKING_SINK" '
            $2 != music && $2 != alert && $2 != master && $2 != parking &&
            $2 != "auto_null" && $2 !~ /^proaudio_player_/ { print $2 }' |
        sort -u
}

state_value() {
    local wanted="$1"
    [[ -f "$STATE_FILE" ]] || return 0
    awk -F= -v wanted="$wanted" '$1 == wanted { print substr($0, index($0,"=")+1); exit }' "$STATE_FILE"
}

find_physical_sink() {
    if [[ "$PHYSICAL_SINK" != "AUTO" ]]; then
        sink_available "$PHYSICAL_SINK" || return 1
        printf '%s\n' "$PHYSICAL_SINK"
        return 0
    fi

    local current
    current="$(state_value PHYSICAL)"
    if [[ -n "$current" ]] && sink_available "$current"; then
        printf '%s\n' "$current"
        return 0
    fi
    physical_candidates | head -n 1
}

write_state() {
    local physical="$1" output_target="$2"
    local temporary="${STATE_FILE}.tmp"
    {
        printf 'GRAPH_BACKEND=pipewire\n'
        printf 'PHYSICAL=%s\n' "$physical"
        printf 'OUTPUT_TARGET=%s\n' "$output_target"
        printf 'MUSIC_SINK=%s\n' "$MUSIC_SINK"
        printf 'ALERT_SINK=%s\n' "$ALERT_SINK"
        printf 'MASTER_SINK=%s\n' "$MASTER_SINK"
        printf 'PARKING_SINK=%s\n' "$PARKING_SINK"
    } >"$temporary"
    mv -f -- "$temporary" "$STATE_FILE"
}

wait_for_sink() {
    local sink="$1"
    local i
    for ((i=0; i<100; i++)); do
        if sink_available "$sink"; then
            return 0
        fi
        sleep 0.1
    done
    echo "PipeWire sink '$sink' не з'явився" >&2
    return 1
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

alsa_card_for_sink() {
    local physical="$1" id
    id="$(node_id "$physical" || true)"
    [[ "$id" =~ ^[0-9]+$ ]] || return 1
    wpctl inspect "$id" 2>/dev/null | awk -F= '
        /(^|[[:space:]])(api\.alsa\.card|alsa\.card)[[:space:]]*=/ {
            value=$2
            gsub(/["[:space:]]/, "", value)
            if (value ~ /^[0-9]+$/) { print value; exit }
        }'
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
    local physical="$1" id
    if [[ "$OUTPUT_VOLUME_PERCENT" != "100" ]]; then
        echo "OUTPUT_VOLUME_PERCENT має бути 100: фізичний PipeWire sink є фіксованим unity stage" >&2
        return 1
    fi
    id="$(node_id "$physical" || true)"
    [[ "$id" =~ ^[0-9]+$ ]] || return 1
    wpctl set-mute "$id" 1
    prepare_hardware_mixer "$physical"
    wpctl set-volume "$id" 1.0
    wpctl set-mute "$id" 0
}

unlink_stereo() {
    local source="$1" target="$2"
    pw-link -d "$source:monitor_FL" "$target:playback_FL" >/dev/null 2>&1 || true
    pw-link -d "$source:monitor_FR" "$target:playback_FR" >/dev/null 2>&1 || true
}

link_stereo() {
    local source="$1" target="$2"
    pw-link "$source:monitor_FL" "$target:playback_FL"
    if ! pw-link "$source:monitor_FR" "$target:playback_FR"; then
        pw-link -d "$source:monitor_FL" "$target:playback_FL" >/dev/null 2>&1 || true
        return 1
    fi
}

connect_internal_graph() {
    unlink_stereo "$MUSIC_SINK" "$MASTER_SINK"
    unlink_stereo "$ALERT_SINK" "$MASTER_SINK"
    if ! link_stereo "$MUSIC_SINK" "$MASTER_SINK"; then
        return 1
    fi
    if ! link_stereo "$ALERT_SINK" "$MASTER_SINK"; then
        unlink_stereo "$MUSIC_SINK" "$MASTER_SINK"
        return 1
    fi
}

initialize_logical_levels() {
    local sink id
    for sink in "$MUSIC_SINK" "$ALERT_SINK" "$MASTER_SINK"; do
        id="$(node_id "$sink" || true)"
        [[ "$id" =~ ^[0-9]+$ ]] || return 1
        wpctl set-volume "$id" 1.0
        wpctl set-mute "$id" 0
    done
    id="$(node_id "$MUSIC_SINK" || true)"
    wpctl set-default "$id" >/dev/null 2>&1 || true
}

validate_audio_bus_config() {
    if [[ "$SAMPLE_RATE" != "48000" ]]; then
        echo "Pure PipeWire appliance graph currently requires SAMPLE_RATE=48000" >&2
        return 1
    fi
    if [[ "$AUDIO_CHANNELS" != "2" ]]; then
        echo "Pure PipeWire appliance graph currently requires AUDIO_CHANNELS=2" >&2
        return 1
    fi
}

wait_for_logical_graph() {
    wait_for_sink "$PARKING_SINK"
    wait_for_sink "$MASTER_SINK"
    wait_for_sink "$MUSIC_SINK"
    wait_for_sink "$ALERT_SINK"
}

start_buses() {
    validate_audio_bus_config
    require_pipewire
    wait_for_logical_graph

    local physical output_target
    physical="$(find_physical_sink || true)"
    output_target="${physical:-$PARKING_SINK}"

    if [[ -n "$physical" ]]; then
        prepare_physical_sink "$physical"
    fi

    if ! connect_internal_graph; then
        echo "Не вдалося створити MUSIC/ALERT -> MASTER PipeWire graph" >&2
        return 1
    fi
    initialize_logical_levels

    unlink_stereo "$MASTER_SINK" "$PARKING_SINK"
    if [[ -n "$physical" ]]; then
        unlink_stereo "$MASTER_SINK" "$physical"
    fi
    if ! link_stereo "$MASTER_SINK" "$output_target"; then
        echo "Не вдалося підключити MASTER до '$output_target'" >&2
        return 1
    fi

    write_state "$physical" "$output_target"
    echo "MUSIC + ALERT -> $MASTER_SINK -> $output_target (native PipeWire graph)"
}

switch_output() {
    validate_audio_bus_config
    require_pipewire
    wait_for_logical_graph

    local physical output_target old_target master_id master_was_muted=0
    physical="$(find_physical_sink || true)"
    output_target="${physical:-$PARKING_SINK}"
    old_target="$(state_value OUTPUT_TARGET)"

    if [[ "$old_target" == "$output_target" ]] && sink_available "$output_target"; then
        return 0
    fi

    if [[ -n "$physical" ]]; then
        prepare_physical_sink "$physical"
    fi

    master_id="$(node_id "$MASTER_SINK" || true)"
    [[ "$master_id" =~ ^[0-9]+$ ]] || return 1
    if wpctl get-volume "$master_id" 2>/dev/null | grep -q '\[MUTED\]'; then
        master_was_muted=1
    fi
    wpctl set-mute "$master_id" 1

    unlink_stereo "$MASTER_SINK" "$output_target"
    if ! link_stereo "$MASTER_SINK" "$output_target"; then
        unlink_stereo "$MASTER_SINK" "$output_target"
        ((master_was_muted == 1)) || wpctl set-mute "$master_id" 0 >/dev/null 2>&1 || true
        echo "Не вдалося підключити MASTER до '$output_target'; попередній маршрут залишено" >&2
        return 1
    fi

    if [[ -n "$old_target" && "$old_target" != "$output_target" ]]; then
        unlink_stereo "$MASTER_SINK" "$old_target"
    fi

    write_state "$physical" "$output_target"
    ((master_was_muted == 1)) || wpctl set-mute "$master_id" 0
    echo "Фінальний PipeWire вихід перемкнено ${old_target:-<none>} -> $output_target"
}

stop_buses() {
    local output_target
    output_target="$(state_value OUTPUT_TARGET)"
    [[ -n "$output_target" ]] && unlink_stereo "$MASTER_SINK" "$output_target"
    unlink_stereo "$MUSIC_SINK" "$MASTER_SINK"
    unlink_stereo "$ALERT_SINK" "$MASTER_SINK"
    rm -f -- "$STATE_FILE"
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
