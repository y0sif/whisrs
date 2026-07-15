//! Scripted repro harness for issue #55: local whisper streaming produces
//! repeated/invented text, worse when the speaker pauses.
//!
//! Feeds a WAV file through `LocalWhisperBackend::transcribe_stream` exactly
//! the way the daemon does (i16 16 kHz mono chunks over an mpsc channel),
//! joins the emitted text with the daemon's spacing rule, then runs a
//! repetition detector over the final transcript.
//!
//! Usage:
//!   cargo run --release --example issue55_stream_check --features local-whisper -- \
//!       [WAV_PATH] [MODEL_PATH] [TRUTH_PATH]
//!
//! Defaults (overridable via env):
//!   WAV_PATH    fixtures/issue55.wav                        ($WHISRS_ISSUE55_WAV)
//!   MODEL_PATH  ~/.local/share/whisrs/models/ggml-base.en.bin ($WHISRS_ISSUE55_MODEL)
//!   TRUTH_PATH  fixtures/issue55.txt if present             ($WHISRS_ISSUE55_TRUTH)
//!
//! Extra env knobs:
//!   WHISRS_ISSUE55_SEGMENTATION       "silence" (default) | "window"
//!   WHISRS_ISSUE55_PHRASE_SILENCE_MS  phrase split threshold (default 400)
//!   WHISRS_ISSUE55_MIN_COVERAGE       if set (e.g. "0.8"), fail when less
//!                                     than that fraction of ground-truth
//!                                     words appears in the transcript
//!
//! Exit code: 0 if the transcript is clean (and coverage passes when
//! required), 1 otherwise — so once the bug is fixed this doubles as a
//! regression check.

use std::collections::HashMap;
use std::process::ExitCode;

use tokio::sync::mpsc;
use whisrs::transcription::{
    local_whisper::LocalWhisperBackend, TranscriptionBackend, TranscriptionConfig,
};

/// 100 ms of 16 kHz mono audio — the granularity the daemon's capture layer
/// delivers. The streaming loop is buffer-length driven, so no real-time
/// pacing is needed: all windows still fire in order.
const CHUNK_SAMPLES: usize = 1600;

/// N-gram sizes checked by the repetition detector.
const NGRAM_RANGE: std::ops::RangeInclusive<usize> = 3..=6;

fn default_model_path() -> String {
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("whisrs/models/ggml-base.en.bin")
        .to_string_lossy()
        .into_owned()
}

/// Mirror of the daemon's `leads_with_punct` (src/daemon/main.rs): batches
/// that lead with punctuation don't get a space inserted before them.
fn leads_with_punct(text: &str) -> bool {
    text.starts_with([
        '.', ',', '!', '?', ';', ':', ')', ']', '}', '。', '，', '！', '？', '；', '：', '）',
        '】', '｝', '…', '\u{2019}', '\u{201D}',
    ])
}

/// Join emitted chunks the way the daemon's typing task does: each chunk is
/// one batch; a space is inserted between batches unless the boundary already
/// has one or the batch leads with punctuation.
fn join_daemon_style(chunks: &[String]) -> String {
    let mut full = String::new();
    for batch in chunks {
        if batch.is_empty() {
            continue;
        }
        if full.is_empty()
            || batch.starts_with(' ')
            || full.ends_with(' ')
            || leads_with_punct(batch)
        {
            full.push_str(batch);
        } else {
            full.push(' ');
            full.push_str(batch);
        }
    }
    full
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

fn ngram_counts(words: &[String], n: usize) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for win in words.windows(n) {
        *counts.entry(win.join(" ")).or_default() += 1;
    }
    counts
}

/// N-grams that occur more often in the transcript than in the ground truth
/// (or more than once when no ground truth is available).
fn repeated_ngrams(
    transcript: &[String],
    truth: Option<&[String]>,
    n: usize,
) -> Vec<(String, usize, usize)> {
    let counts = ngram_counts(transcript, n);
    let truth_counts = truth.map(|t| ngram_counts(t, n));
    let mut out: Vec<(String, usize, usize)> = counts
        .into_iter()
        .filter_map(|(gram, count)| {
            let allowed = truth_counts
                .as_ref()
                .map_or(1, |tc| tc.get(&gram).copied().unwrap_or(0).max(1));
            (count > allowed).then_some((gram, count, allowed))
        })
        .collect();
    out.sort();
    out
}

/// Multiset difference: words in the transcript that exceed their ground-truth
/// occurrence count (invented/inserted words).
fn inserted_words(transcript: &[String], truth: &[String]) -> Vec<(String, usize)> {
    let mut budget: HashMap<&str, usize> = HashMap::new();
    for w in truth {
        *budget.entry(w.as_str()).or_default() += 1;
    }
    let mut extra: HashMap<String, usize> = HashMap::new();
    for w in transcript {
        match budget.get_mut(w.as_str()) {
            Some(c) if *c > 0 => *c -= 1,
            _ => *extra.entry(w.clone()).or_default() += 1,
        }
    }
    let mut out: Vec<(String, usize)> = extra.into_iter().collect();
    out.sort();
    out
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
        "expected 16 kHz mono s16 WAV, got {} Hz {} ch {} bit {:?} — \
         regenerate with scripts/gen-issue55-fixture.sh",
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
        .or_else(|| std::env::var("WHISRS_ISSUE55_WAV").ok())
        .unwrap_or_else(|| "fixtures/issue55.wav".to_string());
    let model_path = args
        .next()
        .or_else(|| std::env::var("WHISRS_ISSUE55_MODEL").ok())
        .unwrap_or_else(default_model_path);
    let truth_path = args
        .next()
        .or_else(|| std::env::var("WHISRS_ISSUE55_TRUTH").ok())
        .or_else(|| {
            let default = "fixtures/issue55.txt";
            std::path::Path::new(default)
                .exists()
                .then(|| default.to_string())
        });

    let segmentation =
        std::env::var("WHISRS_ISSUE55_SEGMENTATION").unwrap_or_else(|_| "silence".to_string());
    let phrase_silence_ms: u64 = std::env::var("WHISRS_ISSUE55_PHRASE_SILENCE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    let min_coverage: Option<f64> = std::env::var("WHISRS_ISSUE55_MIN_COVERAGE")
        .ok()
        .and_then(|v| v.parse().ok());

    let samples = read_wav_samples(&wav_path)?;
    println!(
        "== issue #55 stream check ==\n\
         wav:   {wav_path} ({:.1}s, {} samples)\n\
         model: {model_path}\n\
         mode:  {segmentation} (phrase_silence_ms={phrase_silence_ms})",
        samples.len() as f64 / 16_000.0,
        samples.len(),
    );

    let backend =
        LocalWhisperBackend::new(model_path).with_segmentation(&segmentation, phrase_silence_ms);
    let config = TranscriptionConfig {
        language: "en".to_string(),
        model: "base.en".to_string(),
        prompt: None,
    };

    let (audio_tx, audio_rx) = mpsc::channel::<Vec<i16>>(256);
    let (text_tx, mut text_rx) = mpsc::channel::<String>(64);

    let backend_task =
        tokio::spawn(async move { backend.transcribe_stream(audio_rx, text_tx, &config).await });
    let collector = tokio::spawn(async move {
        let mut chunks: Vec<String> = Vec::new();
        while let Some(t) = text_rx.recv().await {
            chunks.push(t);
        }
        chunks
    });

    for chunk in samples.chunks(CHUNK_SAMPLES) {
        if audio_tx.send(chunk.to_vec()).await.is_err() {
            break; // backend hung up (e.g. model failed to load)
        }
    }
    drop(audio_tx);

    backend_task.await??;
    let chunks = collector.await?;

    println!("\n-- emitted batches ({}) --", chunks.len());
    for (i, c) in chunks.iter().enumerate() {
        println!("[{i:>2}] {c:?}");
    }

    let transcript = join_daemon_style(&chunks);
    println!("\n-- final joined transcript --\n{transcript}");

    let truth_text = match &truth_path {
        Some(p) => Some(std::fs::read_to_string(p)?),
        None => None,
    };
    let words = normalize_words(&transcript);
    let truth_words = truth_text.as_deref().map(normalize_words);

    println!("\n-- repetition detector (n-grams, case/punct-insensitive) --");
    let mut repetition_found = false;
    for n in NGRAM_RANGE {
        let reps = repeated_ngrams(&words, truth_words.as_deref(), n);
        for (gram, count, allowed) in &reps {
            println!("REPEATED {n}-gram x{count} (expected <= {allowed}): {gram:?}");
            repetition_found = true;
        }
    }
    if !repetition_found {
        println!("no repeated n-grams detected");
    }

    if let Some(truth_words) = &truth_words {
        println!("\n-- inserted words vs ground truth ({:?}) --", truth_path);
        let inserted = inserted_words(&words, truth_words);
        if inserted.is_empty() {
            println!("no inserted words");
        } else {
            for (w, count) in &inserted {
                println!("INSERTED x{count}: {w:?}");
            }
        }
    }

    let mut coverage_failed = false;
    if let Some(truth_words) = &truth_words {
        let coverage = truth_coverage(&words, truth_words);
        println!("\n-- ground-truth coverage --");
        println!(
            "{:.1}% of {} truth words present in transcript",
            coverage * 100.0,
            truth_words.len()
        );
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

    if repetition_found {
        println!("\nRESULT: FAIL — repetition detected (issue #55 reproduced)");
        Ok(ExitCode::from(1))
    } else if coverage_failed {
        println!("\nRESULT: FAIL — transcript does not cover the full utterance");
        Ok(ExitCode::from(1))
    } else {
        println!("\nRESULT: OK — transcript is repetition-free");
        Ok(ExitCode::SUCCESS)
    }
}
