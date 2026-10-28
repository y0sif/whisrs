#!/usr/bin/env bash
# whisrs check-vad: scripted gate for the speech detector (audio-silence-gate).
#
# Generates the synthetic fixtures (if missing) and runs the shipped
# detectors over them (examples/vad_check.rs of audio-silence-gate, close
# profile, speech at -40 dBFS), with the 0.1 RMS rule printed beside them:
#
#   clean                    F1 >= 0.95; auto-stop +1.9 to +2.6 s after speech
#   quiet, speech -60 dBFS   recall >= 0.80; auto-stop +1.9 to +2.6 s; gate passes
#   pink, brown at +10/+5/0  noise-only frames flagged <= 2%; auto-stop
#                            +1.9 to +2.6 s in each of 6 noise draws
#   pink switching on at 1 s auto-stop not armed before the speech, never early
#                            (from digital silence, and from pink at -60 dBFS)
#   babble +20 dB SNR        auto-stop +1.9 to +3.0 s in each of 6 draws
#                            (babble at +10 dB and louder is printed, not gated)
#   lone "yes"               gate passes; auto-stop arms and fires
#   "Oh!" +20 and +26 dB,    auto-stop never before the sentences end;
#   then two sentences       recall of the sentences >= 0.80
#   pink only, -40 dBFS      gate "without speech"; auto-stop never arms
#   key clicks, -20/-30 dBFS gate "without speech"; auto-stop never arms
#   real soft voice          only with the local fixtures/vad/real-soft.wav (the
#                            maintainer's take, never committed; skipped without
#                            it): recording recall of the labelled soft speech
#                            >= 0.70 and >= 0.90 of it sent; three dictation cuts
#                            arm and stop +1.8 to +2.6 s after the last soft word
#   quiet, speech -70 dBFS   gate "too quiet"
#   CPU                      earshot faster than 100x real time
#
# Run it before accepting a new earshot version (the crate pins ~1.2.2) or
# a change to the detector's thresholds.
#
# Exit code: 0 = every gated row passes, 1 = at least one failed,
# 2 = bad fixtures.
#
# Usage:
#   ./scripts/check-vad.sh                    # synthetic fixtures only
#   ./scripts/check-vad.sh --real a.wav ...   # also print 16 kHz mono tracks (never gated)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURES="$REPO_ROOT/fixtures/vad"

for name in clean autostop yes burst pink brown babble; do
    if [ ! -f "$FIXTURES/$name.wav" ]; then
        "$REPO_ROOT/scripts/gen-vad-fixture.sh"
        break
    fi
done

cd "$REPO_ROOT"
status=0
nice -n 15 cargo run --release -j 4 -p audio-silence-gate --example vad_check -- \
    "$FIXTURES" "$@" || status=$?

echo
if [ "$status" -eq 0 ]; then
    echo "check-vad: OK"
else
    echo "check-vad: FAIL (exit $status)"
fi
exit "$status"
