//! Scripted repro harness for issue #208: local whisper with
//! `segmentation = "none"` fell into a repetition loop on long dictations.
//!
//! Transcribes a WAV file exactly the way the daemon does for "none":
//! `LocalWhisperBackend` with that segmentation, one `transcribe` call on the
//! whole recording as a WAV. Then it looks for a loop in the transcript and,
//! given a ground truth, measures how much of it the transcript covers and
//! fails a transcript with more than 1.2x its words (repeated or invented
//! text that no back-to-back loop shows).
//!
//! Usage:
//!   cargo run --release --example issue208_batch_check --features local-whisper -- \
//!       [WAV_PATH] [MODEL_PATH] [TRUTH_PATH]
//!
//! Defaults (overridable via env):
//!   WAV_PATH    fixtures/issue208_long.wav                              ($WHISRS_ISSUE208_WAV)
//!   MODEL_PATH  ~/.local/share/whisrs/models/ggml-large-v3-turbo-q5_0.bin ($WHISRS_ISSUE208_MODEL)
//!   TRUTH_PATH  fixtures/issue208_long.txt if present                   ($WHISRS_ISSUE208_TRUTH)
//!
//! Extra env knobs:
//!   WHISRS_ISSUE208_LANG          language (default "en")
//!   WHISRS_ISSUE208_PROMPT        the dictation prompt (default none)
//!   WHISRS_ISSUE208_VAD           `[audio] vad`: "earshot" (default) | "rms"
//!   WHISRS_ISSUE208_MIN_COVERAGE  if set (e.g. "0.9"), fail when less than
//!                                 that fraction of ground-truth words
//!                                 appears in the transcript
//!
//! Exit code: 0 if the transcript has no loop and not too many words (and
//! coverage passes when required), 1 otherwise or on an error.

use std::collections::HashMap;
use std::process::ExitCode;
use std::time::Instant;

use whisrs::transcription::{
    local_whisper::LocalWhisperBackend, TranscriptionBackend, TranscriptionConfig,
};

/// A phrase repeated back to back at least this many times is a loop.
const LOOP_MIN_REPEATS: usize = 3;
/// Phrase lengths, in words, the loop detector tries: up to a long sentence.
const LOOP_PHRASE_WORDS: std::ops::RangeInclusive<usize> = 2..=40;
/// A transcript with more words than this times the ground truth's repeats
/// or invents text, even when no phrase repeats back to back.
const MAX_TRUTH_WORD_RATIO: f64 = 1.2;

fn default_model_path() -> String {
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("whisrs/models/ggml-large-v3-turbo-q5_0.bin")
        .to_string_lossy()
        .into_owned()
}

/// Case- and punctuation-insensitive word list (apostrophes kept).
fn normalize_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

struct Loop {
    start_word: usize,
    phrase: String,
    repeats: usize,
}

/// The longest back-to-back run of one phrase (by words covered), if it
/// repeats at least [`LOOP_MIN_REPEATS`] times.
fn find_loop(words: &[String]) -> Option<Loop> {
    let mut best: Option<(usize, Loop)> = None;
    for n in LOOP_PHRASE_WORDS {
        let mut i = 0;
        while i + n <= words.len() {
            let mut repeats = 1;
            while i + (repeats + 1) * n <= words.len()
                && words[i..i + n] == words[i + repeats * n..i + (repeats + 1) * n]
            {
                repeats += 1;
            }
            let covered = repeats * n;
            if repeats >= LOOP_MIN_REPEATS && best.as_ref().is_none_or(|(c, _)| covered > *c) {
                best = Some((
                    covered,
                    Loop {
                        start_word: i,
                        phrase: words[i..i + n].join(" "),
                        repeats,
                    },
                ));
            }
            i += if repeats > 1 { repeats * n } else { 1 };
        }
    }
    best.map(|(_, l)| l)
}

/// Fraction of ground-truth words present in the transcript (multiset:
/// each transcript word can satisfy at most one truth occurrence).
fn truth_coverage(transcript: &[String], truth: &[String]) -> f64 {
    if truth.is_empty() {
        return 1.0;
    }
    let mut available: HashMap<&str, usize> = HashMap::new();
    for w in transcript {
        *available.entry(w.as_str()).or_default() += 1;
    }
    let mut matched = 0usize;
    for w in truth {
        if let Some(c) = available.get_mut(w.as_str()) {
            if *c > 0 {
                *c -= 1;
                matched += 1;
            }
        }
    }
    matched as f64 / truth.len() as f64
}

fn read_wav_samples(path: &str) -> anyhow::Result<Vec<i16>> {
    let reader = hound::WavReader::open(path)
        .map_err(|e| anyhow::anyhow!("failed to open WAV {path}: {e}"))?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 1
            && spec.sample_rate == 16_000
            && spec.bits_per_sample == 16
            && spec.sample_format == hound::SampleFormat::Int,
        "expected 16 kHz mono s16 WAV, got {} Hz {} ch {} bit {:?}: \
         regenerate with scripts/gen-issue208-fixture.sh",
        spec.sample_rate,
        spec.channels,
        spec.bits_per_sample,
        spec.sample_format,
    );
    Ok(reader.into_samples::<i16>().collect::<Result<_, _>>()?)
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let wav_path = args
        .next()
        .or_else(|| std::env::var("WHISRS_ISSUE208_WAV").ok())
        .unwrap_or_else(|| "fixtures/issue208_long.wav".to_string());
    let model_path = args
        .next()
        .or_else(|| std::env::var("WHISRS_ISSUE208_MODEL").ok())
        .unwrap_or_else(default_model_path);
    let truth_path = args
        .next()
        .or_else(|| std::env::var("WHISRS_ISSUE208_TRUTH").ok())
        .or_else(|| {
            let default = "fixtures/issue208_long.txt";
            std::path::Path::new(default)
                .exists()
                .then(|| default.to_string())
        });

    let language = std::env::var("WHISRS_ISSUE208_LANG").unwrap_or_else(|_| "en".to_string());
    let prompt = std::env::var("WHISRS_ISSUE208_PROMPT")
        .ok()
        .filter(|p| !p.is_empty());
    let min_coverage: Option<f64> = std::env::var("WHISRS_ISSUE208_MIN_COVERAGE")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| {
            v.parse().map_err(|e| {
                anyhow::anyhow!(
                    "WHISRS_ISSUE208_MIN_COVERAGE={v:?} is not a fraction such as \"0.9\": {e}"
                )
            })
        })
        .transpose()?;
    anyhow::ensure!(
        min_coverage.is_none() || truth_path.is_some(),
        "WHISRS_ISSUE208_MIN_COVERAGE needs a ground-truth file"
    );
    let vad = match std::env::var("WHISRS_ISSUE208_VAD") {
        Ok(v) => whisrs::parse_vad(&v)
            .ok_or_else(|| anyhow::anyhow!("WHISRS_ISSUE208_VAD={v:?} is not earshot or rms"))?,
        Err(_) => Default::default(),
    };

    let samples = read_wav_samples(&wav_path)?;
    println!(
        "== issue #208 batch check ==\n\
         wav:    {wav_path} ({:.1}s)\n\
         model:  {model_path}\n\
         lang:   {language}\n\
         prompt: {prompt:?}\n\
         vad:    {}",
        samples.len() as f64 / 16_000.0,
        whisrs::vad_name(vad),
    );

    let backend = LocalWhisperBackend::new(model_path).with_segmentation("none", 400);
    anyhow::ensure!(
        !backend.supports_streaming(),
        "segmentation \"none\" should take the batch path"
    );
    let config = TranscriptionConfig {
        language,
        model: "local".to_string(),
        prompt,
        keyterms: Vec::new(),
        diarize: false,
        vad,
    };

    let wav = whisrs::audio::wav::encode_wav(&samples)?;
    let began = Instant::now();
    let transcript = backend.transcribe(&wav, &config).await?;
    let elapsed = began.elapsed().as_secs_f64();

    println!("\n-- transcript --\n{transcript}");

    let words = normalize_words(&transcript);
    println!("\n-- summary --");
    println!("decode: {elapsed:.1}s, {} words", words.len());

    let found = find_loop(&words);
    match &found {
        Some(l) => println!(
            "LOOP: {:?} x{} back to back, from word {} of {}",
            l.phrase,
            l.repeats,
            l.start_word,
            words.len()
        ),
        None => println!("no back-to-back loop (>= {LOOP_MIN_REPEATS} repeats)"),
    }

    let mut coverage_failed = false;
    let mut too_many_words = false;
    if let Some(p) = &truth_path {
        let truth = normalize_words(&std::fs::read_to_string(p)?);
        let coverage = truth_coverage(&words, &truth);
        println!(
            "coverage: {:.1}% of {} truth words",
            coverage * 100.0,
            truth.len()
        );
        if words.len() as f64 > MAX_TRUTH_WORD_RATIO * truth.len() as f64 {
            println!(
                "WORD COUNT FAIL: {} words, more than {MAX_TRUTH_WORD_RATIO}x the {} truth words",
                words.len(),
                truth.len()
            );
            too_many_words = true;
        }
        if let Some(min) = min_coverage {
            if coverage < min {
                println!(
                    "COVERAGE FAIL: {:.1}% < required {:.1}%",
                    coverage * 100.0,
                    min * 100.0
                );
                coverage_failed = true;
            }
        }
    }

    if found.is_some() {
        println!("\nRESULT: FAIL, repetition loop (issue #208 reproduced)");
        Ok(ExitCode::from(1))
    } else if too_many_words {
        println!("\nRESULT: FAIL, more words than the dictation (repeated or invented text)");
        Ok(ExitCode::from(1))
    } else if coverage_failed {
        println!("\nRESULT: FAIL, transcript does not cover the dictation");
        Ok(ExitCode::from(1))
    } else {
        println!("\nRESULT: OK, no loop");
        Ok(ExitCode::SUCCESS)
    }
}
