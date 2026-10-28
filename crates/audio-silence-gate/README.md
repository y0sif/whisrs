# audio-silence-gate

Speech detection for audio capture: is anyone speaking, and how loud? A learned
voice activity detector ([earshot](https://crates.io/crates/earshot)) behind
warm-up, level floors, onset confirmation, hysteresis and hangover, with a
streaming auto-stop, a whole-buffer gate, a per-frame speech mask and a level
meter built on it. The plain RMS test of 0.1 stays beside it.

## The detector

`SpeechDetector` scores every 16 ms frame of 16 kHz `i16` audio with earshot,
then decides per frame, in order (levels are in dBFS, decibels relative to
full scale: 0 is the loudest a sample can be, and a normal voice near a mic
sits around -40):

1. the first 160 ms after `new` or `reset` are never speech (an abrupt noise
   onset scores high while the model settles);
2. a frame under an absolute level floor, or too far under the running speech
   level, is not speech whatever its score;
3. speech starts after 2 frames in a row at or above the threshold (0.55), so
   a one-frame click never starts it;
4. once in speech, frames at or above the release score keep it;
5. speech ends only after 96 ms under the release score, which bridges the dips
   inside words;
6. a frame that stands 6 dB or more over the background (the quietest frame of
   the last 240 ms of sound, both measured after a pre-emphasis that turns down
   a mic's rumble), as the frame before it did, needs only the soft threshold
   (0.25) in 3 and 4. earshot scores a soft voice 0.2 to 0.5, under 0.55, while
   steady noise never stands that clear and a key click never does twice in a
   row. Speech voiced only that way counts half toward arming auto-stop and
   passing the gate, and only within its run of speech (304 ms without a
   voiced frame forgets it). A noise switching on stands clear of the
   background for at most 240 ms, so it can mark about 300 ms as speech, but
   on that rule alone it cannot arm auto-stop or pass the gate, and neither
   can several such onsets apart (a fan cycling). A noise earshot itself
   scores as speech still counts in full.

Feed it chunks of any size; it calls back once per completed frame.

```rust
use audio_silence_gate::{SpeechDetector, Vad, VadConfig};

let mut detector = SpeechDetector::new(VadConfig::close(Vad::Earshot), 16_000);
detector.feed(&chunk, |frame| {
    if frame.speech {
        println!("speech at {} ({:.1} dBFS)", frame.start_sample, frame.dbfs);
    }
});
```

At any sample rate other than 16 kHz, or with `Vad::Rms`, a frame is speech
when its RMS reaches `VadConfig::rms_threshold` instead: a documented fallback,
never a panic.

## Profiles

The right level floor depends on who may speak:

- `VadConfig::close`: one user near the mic (dictation, voice commands). Frames
  under -65 dBFS, or more than 18 dB under the user's running level, are not
  speech, so a TV or a colleague behind the user does not hold a recording open.
  That relative floor never rises above the RMS test's threshold (-50.5 dBFS),
  so a laugh at the mic cannot push the user's normal speech under it.
- `VadConfig::open`: any voice counts (a meeting track, system audio). Only
  frames under -80 dBFS are rejected, since dropping a quiet voice there loses
  transcript.

The fields are public for tuning. The shipped values are tuned with the
benchmark in whisrs'
[`scripts/check-vad.sh`](https://github.com/y0sif/whisrs/blob/main/scripts/check-vad.sh).

## Real-time auto-stop

```rust
use audio_silence_gate::{AutoStopDetector, Vad, VadConfig};

let mut detector = AutoStopDetector::with_vad(
    VadConfig::close(Vad::Earshot),
    2_000,   // 2 seconds of silence after speech triggers stop; 0 never fires
    16_000,  // sample rate (Hz)
);

loop {
    let chunk = capture.next_chunk();
    if detector.feed(&chunk) {
        println!("silence timeout, stopping: {:?}", detector.levels());
        break;
    }
}
detector.reset(); // ready for next utterance
```

It arms once 160 ms of speech was accepted in total (320 ms if accepted only
through the soft threshold, within one run of speech), so a short noise onset
cannot arm it while a lone "yes" or soft talk does. `heard_voice()` turns true
earlier, on the first frame the detector calls speech before the level floors
and that count: a sign that someone spoke even when the voice is too quiet to
arm it. `AutoStopDetector::new(threshold, ..)` keeps the 0.1 RMS rule.

## Batch gate

Filter out unusable recordings before they reach a transcription API, and say why:

```rust
use audio_silence_gate::{speech_gate, GateReason, Vad, VadConfig};

let gate = speech_gate(&samples, 16_000, 300, &VadConfig::close(Vad::Earshot));
match gate.reason {
    None => send_to_transcription_api(&samples),
    Some(GateReason::Silent) => eprintln!("digital silence (peak {:.0} dB)", gate.levels.peak_dbfs),
    Some(GateReason::TooQuiet) => eprintln!("a voice, too faint: {:?} dBFS", gate.levels.speech_dbfs),
    Some(GateReason::NoSpeech) => eprintln!("sound, no speech: background {:?} dBFS", gate.levels.noise_dbfs),
    // Empty, TooShort, Invalid; `GateReason` is `#[non_exhaustive]`.
    Some(other) => eprintln!("skipped: {}", other.as_str()),
}
```

A buffer whose peak is digital silence is rejected before the model runs, so a
muted mic never pays for it. `audio_gate_reason` keeps the 0.1 RMS gate.

## Speech mask and levels

`SpeechMask` streams per-frame speech flags over a long track (one flag per
`frame_len` samples) without loading it. It runs at 16 kHz only.
`LevelMeter` runs inside every detector; `levels()` gives the speech level,
the background level (median of the non-speech frames), the SNR
(signal-to-noise ratio: how far the speech is above the background, in dB),
the share of clipped samples (runs of 3 or more at the rail; single
full-scale peaks do not count), the DC offset (a constant shift some mics
add), the share of speech, the peak and the duration. What counts as too
quiet or too noisy is left to the caller.

## When to use audio-silence-gate vs Silero VAD

|                   | audio-silence-gate (earshot)       | audio-silence-gate (`Vad::Rms`) | Silero VAD             |
|-------------------|------------------------------------|---------------------------------|------------------------|
| Dependencies      | earshot (std only, none of its own) | same                           | ONNX runtime           |
| Cost              | about 11 microseconds per 16 ms frame, measured by check-vad.sh on one machine | microseconds | milliseconds |
| Decision delay    | 48 to 64 ms, earshot's own (onset confirmation adds a frame) | one frame | one window |
| Noise             | a steady fan or hum is not speech  | anything loud is speech         | neural                 |
| Best for          | auto-stop and gating on 16 kHz audio | a mic the model handles badly | precise speech segmentation |

earshot has higher recall and lower precision than Silero; the onset
confirmation, hysteresis and hangover make up for it. Its weak spot is quiet
background voices, which the `close` profile's relative floor rejects when
they sit well under the user.

## Installation

```sh
cargo add audio-silence-gate
```

Needs Rust 1.87 or newer (earshot's minimum).

## License

MIT
