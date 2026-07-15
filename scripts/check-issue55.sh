#!/usr/bin/env bash
# whisrs check-issue55 — scripted sanity check for issue #55.
#
# Generates the synthetic dictation fixtures (if missing) and streams them
# through LocalWhisperBackend the same way the daemon does:
#
#   1. issue55.wav          pause-heavy dictation — must transcribe with no
#                           repeated n-grams (the original issue #55 repro).
#   2. issue55_nopause.wav  ~28 s of continuous pause-free speech — must be
#                           transcribed in full (>= 80% ground-truth word
#                           coverage), proving the 20 s max-segment cap emits
#                           instead of stalling.
#
# Exit code: 0 = both checks pass, 1 = at least one failed.
#
# Usage:
#   ./scripts/check-issue55.sh                 # uses ~/.local/share/whisrs/models/ggml-base.en.bin
#   WHISRS_ISSUE55_MODEL=/path/to/model.bin ./scripts/check-issue55.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WAV="$REPO_ROOT/fixtures/issue55.wav"
TRUTH="$REPO_ROOT/fixtures/issue55.txt"
NP_WAV="$REPO_ROOT/fixtures/issue55_nopause.wav"
NP_TRUTH="$REPO_ROOT/fixtures/issue55_nopause.txt"
MODEL="${WHISRS_ISSUE55_MODEL:-$HOME/.local/share/whisrs/models/ggml-base.en.bin}"

if [ ! -f "$WAV" ] || [ ! -f "$NP_WAV" ]; then
    "$REPO_ROOT/scripts/gen-issue55-fixture.sh"
fi

if [ ! -f "$MODEL" ]; then
    echo "Model not found: $MODEL" >&2
    echo "Run 'whisrs setup' to download one, or set WHISRS_ISSUE55_MODEL." >&2
    exit 2
fi

cd "$REPO_ROOT"
cargo build --release --example issue55_stream_check --features local-whisper
BIN="$REPO_ROOT/target/release/examples/issue55_stream_check"

echo
echo "=== 1/2 pause fixture (repetition gate) ==="
pause_status=0
"$BIN" "$WAV" "$MODEL" "$TRUTH" || pause_status=$?

echo
echo "=== 2/2 no-pause fixture (coverage gate, 20s cap) ==="
nopause_status=0
WHISRS_ISSUE55_MIN_COVERAGE=0.8 "$BIN" "$NP_WAV" "$MODEL" "$NP_TRUTH" \
    || nopause_status=$?

echo
echo "=== summary ==="
if [ "$pause_status" -eq 0 ]; then
    echo "pause fixture:    OK"
else
    echo "pause fixture:    FAIL (exit $pause_status)"
fi
if [ "$nopause_status" -eq 0 ]; then
    echo "no-pause fixture: OK"
else
    echo "no-pause fixture: FAIL (exit $nopause_status)"
fi

if [ "$pause_status" -ne 0 ] || [ "$nopause_status" -ne 0 ]; then
    exit 1
fi
exit 0
