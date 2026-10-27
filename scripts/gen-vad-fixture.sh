#!/usr/bin/env bash
# whisrs gen-vad-fixture: synthesize the audio fixtures for scripts/check-vad.sh.
#
# Speech comes from espeak-ng, noise beds from ffmpeg's anoisesrc. The beds
# are mixed into the speech by the check itself (examples/vad_check.rs of
# audio-silence-gate), so every SNR is exact against the speech level.
#
# Output (gitignored, never commit the WAVs), all 16 kHz mono s16:
#   fixtures/vad/clean.wav     2 s dead air, the issue #55 dictation (long pauses), 2 s dead air
#   fixtures/vad/autostop.wav  2 s dead air, one dictation with short pauses, 6 s dead air
#   fixtures/vad/yes.wav       2 s dead air, a lone "yes", 4 s dead air
#   fixtures/vad/pink.wav      90 s of pink noise (fan-like)
#   fixtures/vad/brown.wav     90 s of brown noise
#   fixtures/vad/babble.wav    90 s of four other voices talking at once
#
# Requires: espeak-ng, ffmpeg
#
# Usage:
#   ./scripts/gen-vad-fixture.sh          # writes the fixtures
#   ./scripts/gen-vad-fixture.sh --force  # regenerate even if present

set -euo pipefail

GREEN='\033[32m'
RED='\033[31m'
BOLD='\033[1m'
RESET='\033[0m'

info()  { echo -e "  ${GREEN}${BOLD}$1${RESET} $2"; }
error() { echo -e "  ${RED}$1${RESET}"; }

FORCE=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        -h|--help)
            sed -n '2,20p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            error "Unknown flag: $arg"
            exit 1
            ;;
    esac
done

for tool in espeak-ng ffmpeg ffprobe; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        error "Missing required tool: $tool"
        exit 1
    fi
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$REPO_ROOT/fixtures/vad"
NAMES=(clean autostop yes pink brown babble)

all_present=1
for name in "${NAMES[@]}"; do
    [ -f "$OUT/$name.wav" ] || all_present=0
done
if [ "$all_present" -eq 1 ] && [ "$FORCE" -eq 0 ]; then
    info "Exists:" "$OUT (use --force to regenerate)"
    exit 0
fi

mkdir -p "$OUT"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

ff() { ffmpeg -loglevel error -y "$@"; }

silence() { # silence <seconds> <outfile>
    ff -f lavfi -i anullsrc=channel_layout=mono:sample_rate=16000 -t "$1" -c:a pcm_s16le "$2"
}

say() { # say <voice> <speed> <text> <outfile>
    espeak-ng -v "$1" -s "$2" -w "$WORK_DIR/raw.wav" "$3"
    # Resample espeak-ng output (22.05 kHz) to the pipeline's 16 kHz mono s16.
    ff -i "$WORK_DIR/raw.wav" -ar 16000 -ac 1 -c:a pcm_s16le "$4"
}

# build <out> <lead s> <tail s> <pause s...> -- <segment...>
# Dead air, the segments (en-us, 130 wpm, like gen-issue55-fixture.sh) with
# the given pauses between them, dead air.
build() {
    local out=$1 lead=$2 tail=$3
    shift 3
    local pauses=()
    while [ "$1" != "--" ]; do
        pauses+=("$1")
        shift
    done
    shift
    local tag list i=0
    tag="$(basename "$out" .wav)"
    list="$WORK_DIR/list_$tag.txt"
    silence "$lead" "$WORK_DIR/lead_$tag.wav"
    echo "file '$WORK_DIR/lead_$tag.wav'" > "$list"
    for seg in "$@"; do
        say en-us 130 "$seg" "$WORK_DIR/seg_${tag}_$i.wav"
        echo "file '$WORK_DIR/seg_${tag}_$i.wav'" >> "$list"
        if [ "$i" -lt "${#pauses[@]}" ]; then
            silence "${pauses[$i]}" "$WORK_DIR/gap_${tag}_$i.wav"
            echo "file '$WORK_DIR/gap_${tag}_$i.wav'" >> "$list"
        fi
        i=$((i + 1))
    done
    silence "$tail" "$WORK_DIR/tail_$tag.wav"
    echo "file '$WORK_DIR/tail_$tag.wav'" >> "$list"
    ff -f concat -safe 0 -i "$list" -c copy "$out"
}

# Frame accuracy clip: the issue #55 segments and pauses.
build "$OUT/clean.wav" 2 2 4 3 5 2.5 4 3 -- \
    "Well, let me try to figure out if this is" \
    "might be actually enough to reproduce the problem." \
    "The quick brown fox jumps over the lazy dog." \
    "Today I am testing the local whisper streaming backend." \
    "Sometimes I pause in the middle of a" \
    "sentence and then continue talking after a while." \
    "This is the final sentence of the recording."

# Auto-stop clip: one dictation with only short pauses, then 6 s of tail.
build "$OUT/autostop.wav" 2 6 0.5 0.6 0.4 -- \
    "Please send the quarterly report to the whole team by Friday." \
    "Make sure the numbers in the second table match the invoice." \
    "Then schedule a short call with finance for next week." \
    "And remind me to review the draft before it goes out."

# The shortest answer an agent turn gets: it must still arm auto-stop.
build "$OUT/yes.wav" 2 4 -- "yes"

# Fan-like noise beds.
for color in pink brown; do
    ff -f lavfi -i "anoisesrc=color=$color:sample_rate=16000:amplitude=0.5:seed=7:duration=90" \
        -ac 1 -c:a pcm_s16le "$OUT/$color.wav"
done

# Babble: four other voices with different texts and offsets, looped to 90 s
# and summed without normalizing.
TEXTS=(
    "The committee met on Tuesday to discuss the budget for the coming year, and after a long debate they agreed to postpone the decision until more data was available from the regional offices."
    "My grandmother used to grow tomatoes in the back garden every summer, and she always said that the secret was talking to the plants in the early morning before the sun got too hot."
    "If you take the second exit at the roundabout and follow the road past the old church, you will see the train station on your left just after the petrol station and the bakery."
    "Scientists have discovered a new species of frog in the rainforest, which is able to change the colour of its skin depending on the temperature and the amount of light around it."
)
VOICES=(en-gb+f3 en-us+m4 en-gb-scotland+f2 en-029+m2)
SPEEDS=(160 150 170 145)
DELAYS=(0 1300 2700 4100)
inputs=()
filter=""
for i in 0 1 2 3; do
    say "${VOICES[$i]}" "${SPEEDS[$i]}" "${TEXTS[$i]}" "$WORK_DIR/babble_$i.wav"
    ff -stream_loop -1 -i "$WORK_DIR/babble_$i.wav" -t 95 -c:a pcm_s16le "$WORK_DIR/babble_loop_$i.wav"
    inputs+=(-i "$WORK_DIR/babble_loop_$i.wav")
    filter+="[$i:a]adelay=${DELAYS[$i]}[d$i];"
done
ff "${inputs[@]}" \
    -filter_complex "${filter}[d0][d1][d2][d3]amix=inputs=4:normalize=0:duration=first,atrim=0:90" \
    -ac 1 -ar 16000 -c:a pcm_s16le "$OUT/babble.wav"

for name in "${NAMES[@]}"; do
    duration="$(ffprobe -loglevel error -show_entries format=duration \
        -of default=noprint_wrappers=1:nokey=1 "$OUT/$name.wav")"
    info "Wrote:" "$OUT/$name.wav (${duration%.*}s, 16 kHz mono s16)"
done
