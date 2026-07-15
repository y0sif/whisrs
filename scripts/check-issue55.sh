#!/usr/bin/env bash
# whisrs check-issue55 — scripted sanity check for issue #55.
#
# Generates the synthetic dictation fixture (if missing) and streams it
# through LocalWhisperBackend the same way the daemon does, then scans the
# transcript for repeated n-grams and invented words. No microphone or
# manual dictation needed.
#
# Exit code: 0 = transcript clean, 1 = repetition detected (bug reproduced).
#
# Usage:
#   ./scripts/check-issue55.sh                 # uses ~/.local/share/whisrs/models/ggml-base.en.bin
#   WHISRS_ISSUE55_MODEL=/path/to/model.bin ./scripts/check-issue55.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WAV="$REPO_ROOT/fixtures/issue55.wav"
TRUTH="$REPO_ROOT/fixtures/issue55.txt"
MODEL="${WHISRS_ISSUE55_MODEL:-$HOME/.local/share/whisrs/models/ggml-base.en.bin}"

if [ ! -f "$WAV" ]; then
    "$REPO_ROOT/scripts/gen-issue55-fixture.sh"
fi

if [ ! -f "$MODEL" ]; then
    echo "Model not found: $MODEL" >&2
    echo "Run 'whisrs setup' to download one, or set WHISRS_ISSUE55_MODEL." >&2
    exit 2
fi

cd "$REPO_ROOT"
exec cargo run --release --example issue55_stream_check --features local-whisper -- \
    "$WAV" "$MODEL" "$TRUTH"
