#!/usr/bin/env bash
# whisrs gen-issue55-fixture — synthesize the audio fixture for issue #55.
#
# Builds a 16 kHz mono s16 WAV that mimics the dictation pattern from
# https://github.com/y0sif/whisrs/issues/55: several slow-paced sentences
# separated by 2-6 s silence gaps, including a trailing incomplete phrase
# right before a pause (the golden repro for the repetition bug).
#
# Output (gitignored, never commit the WAV):
#   fixtures/issue55.wav   16 kHz mono s16 test audio (~45 s)
#   fixtures/issue55.txt   ground-truth transcript (one line)
#
# Requires: espeak-ng, ffmpeg
#
# Usage:
#   ./scripts/gen-issue55-fixture.sh          # writes fixtures/issue55.wav
#   ./scripts/gen-issue55-fixture.sh --force  # regenerate even if present

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
            sed -n '2,17p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            error "Unknown flag: $arg"
            exit 1
            ;;
    esac
done

for tool in espeak-ng ffmpeg; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        error "Missing required tool: $tool"
        exit 1
    fi
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE_DIR="$REPO_ROOT/fixtures"
OUT_WAV="$FIXTURE_DIR/issue55.wav"
OUT_TXT="$FIXTURE_DIR/issue55.txt"

if [ -f "$OUT_WAV" ] && [ "$FORCE" -eq 0 ]; then
    info "Exists:" "$OUT_WAV (use --force to regenerate)"
    exit 0
fi

mkdir -p "$FIXTURE_DIR"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

# Spoken segments and the silence gap (seconds) inserted AFTER each one.
# Segments 1 and 5 deliberately stop mid-sentence before a long pause —
# the exact condition issue #55 reports as the worst offender.
SEGMENTS=(
    "Well, let me try to figure out if this is"
    "might be actually enough to reproduce the problem."
    "The quick brown fox jumps over the lazy dog."
    "Today I am testing the local whisper streaming backend."
    "Sometimes I pause in the middle of a"
    "sentence and then continue talking after a while."
    "This is the final sentence of the recording."
)
PAUSES=(4 3 5 2.5 4 3 0.8)

# Slow-ish pacing (default espeak-ng speed is 175 wpm).
ESPEAK_VOICE="en-us"
ESPEAK_SPEED=130

silence() { # silence <seconds> <outfile>
    ffmpeg -loglevel error -y \
        -f lavfi -i anullsrc=channel_layout=mono:sample_rate=16000 \
        -t "$1" -c:a pcm_s16le "$2"
}

CONCAT_LIST="$WORK_DIR/concat.txt"
: > "$CONCAT_LIST"

# Short lead-in silence so speech doesn't start at sample zero.
silence 0.3 "$WORK_DIR/lead.wav"
echo "file '$WORK_DIR/lead.wav'" >> "$CONCAT_LIST"

for i in "${!SEGMENTS[@]}"; do
    raw="$WORK_DIR/raw_$i.wav"
    seg="$WORK_DIR/seg_$i.wav"
    gap="$WORK_DIR/gap_$i.wav"

    espeak-ng -v "$ESPEAK_VOICE" -s "$ESPEAK_SPEED" -w "$raw" "${SEGMENTS[$i]}"
    # Resample espeak-ng output (22.05 kHz) to the pipeline's 16 kHz mono s16.
    ffmpeg -loglevel error -y -i "$raw" -ar 16000 -ac 1 -c:a pcm_s16le "$seg"
    echo "file '$seg'" >> "$CONCAT_LIST"

    silence "${PAUSES[$i]}" "$gap"
    echo "file '$gap'" >> "$CONCAT_LIST"
done

ffmpeg -loglevel error -y -f concat -safe 0 -i "$CONCAT_LIST" -c copy "$OUT_WAV"

# Ground truth: the segments joined with single spaces, one line.
printf '%s\n' "$(IFS=' '; echo "${SEGMENTS[*]}")" > "$OUT_TXT"

DURATION="$(ffprobe -loglevel error -show_entries format=duration \
    -of default=noprint_wrappers=1:nokey=1 "$OUT_WAV")"

info "Wrote:" "$OUT_WAV (${DURATION%.*}s, 16 kHz mono s16)"
info "Wrote:" "$OUT_TXT (ground truth)"
