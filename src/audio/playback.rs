//! Interruptible WAV playback on the default audio output device.
//!
//! Used by the read-selection-aloud feature to play TTS audio of arbitrary
//! sample rate / channel count. Unlike [`crate::audio::feedback`] (mono /
//! 44.1 kHz / 2-second timeout), this plays the clip to completion and can be
//! stopped early via a shared [`AtomicBool`].

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::StreamConfig;
use tracing::{debug, warn};

use crate::WhisrsError;

/// Decoded PCM audio: interleaved f32 samples plus the stream format.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedWav {
    /// Interleaved samples in `[-1.0, 1.0]`, channel-major per frame.
    pub samples: Vec<f32>,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of channels.
    pub channels: u16,
}

impl DecodedWav {
    /// Number of frames (samples per channel).
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }
}

/// Decode WAV bytes into interleaved f32 samples plus format metadata.
///
/// Handles 16-bit integer and 32-bit float WAV files (the formats Groq's TTS
/// endpoint returns); other integer bit depths (8/24/32) are also supported by
/// scaling to f32. This is a pure function with no audio device access so it
/// can be unit-tested.
pub fn decode_wav(wav_bytes: &[u8]) -> Result<DecodedWav, WhisrsError> {
    // Some encoders (ffmpeg/Lavf, which Groq's TTS endpoint uses) stream WAV to
    // a non-seekable output and leave the RIFF and `data` chunk sizes as the
    // 0xFFFFFFFF "unknown length" sentinel. hound then rejects the file
    // ("data chunk length is not a multiple of sample size"). Repair the length
    // fields to the real byte counts before parsing.
    let repaired = repair_streaming_wav(wav_bytes);
    let bytes: &[u8] = repaired.as_deref().unwrap_or(wav_bytes);

    let reader = hound::WavReader::new(Cursor::new(bytes))
        .map_err(|e| WhisrsError::Audio(format!("failed to read WAV header: {e}")))?;
    let spec = reader.spec();

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .into_samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| WhisrsError::Audio(format!("failed to decode float WAV samples: {e}")))?,
        hound::SampleFormat::Int => {
            // Normalize integer samples by the full-scale value for the bit depth.
            let max_amp = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .into_samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max_amp))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    WhisrsError::Audio(format!("failed to decode integer WAV samples: {e}"))
                })?
        }
    };

    if spec.channels == 0 {
        return Err(WhisrsError::Audio("WAV reports zero channels".to_string()));
    }

    Ok(DecodedWav {
        samples,
        sample_rate: spec.sample_rate,
        channels: spec.channels,
    })
}

/// Repair WAV files written to a non-seekable stream, where the `RIFF` and
/// `data` chunk sizes are left as the 0xFFFFFFFF "unknown length" sentinel (or
/// otherwise overrun the buffer). Rewrites both to the real byte counts.
///
/// Returns `Some(fixed_bytes)` when a repair was applied, or `None` when the
/// input is already well-formed or is not a recognizable RIFF/WAVE stream (in
/// which case the caller passes the original bytes through to the parser).
fn repair_streaming_wav(bytes: &[u8]) -> Option<Vec<u8>> {
    const SENTINEL: u32 = 0xFFFF_FFFF;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let read_u32 =
        |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);

    let mut block_align: usize = 0;
    let mut off = 12usize;
    while off + 8 <= bytes.len() {
        let id = &bytes[off..off + 4];
        let len = read_u32(off + 4);
        let payload = off + 8;

        if id == b"fmt " && payload + 16 <= bytes.len() {
            // blockAlign sits at offset 12..14 within the fmt payload.
            block_align = u16::from_le_bytes([bytes[payload + 12], bytes[payload + 13]]) as usize;
        }

        if id == b"data" {
            let avail = bytes.len() - payload;
            // Only repair a clearly-bogus length; leave well-formed files alone.
            if len != SENTINEL && (len as usize) <= avail {
                return None;
            }
            let ba = block_align.max(1);
            let fixed = avail - (avail % ba);
            let mut out = bytes.to_vec();
            out[payload - 4..payload].copy_from_slice(&(fixed as u32).to_le_bytes());
            // RIFF size = everything after the leading 8 bytes, through the data payload.
            let riff = (payload + fixed - 8) as u32;
            out[4..8].copy_from_slice(&riff.to_le_bytes());
            return Some(out);
        }

        // Can't safely walk past a sentinel-length chunk that precedes `data`.
        if len == SENTINEL {
            return None;
        }
        off = off.checked_add(8 + len as usize + (len as usize & 1))?;
    }
    None
}

/// Play WAV-encoded audio on the default output device, blocking until the clip
/// finishes or `stop` is set to `true`.
///
/// Builds a cpal output stream matching the WAV's sample rate and channel count.
/// The stream callback advances through the decoded samples; when they are
/// exhausted (or `stop` is set) it emits silence and signals completion. This
/// function is intended to be run on a blocking task (`spawn_blocking`).
///
/// When `level_tx` is `Some`, a normalized amplitude (0..=1) computed from each
/// emitted buffer is published so a speaking overlay can react to the audio.
/// The watch channel coalesces, so no throttling is needed.
pub fn play_wav(
    wav_bytes: &[u8],
    stop: Arc<AtomicBool>,
    level_tx: Option<tokio::sync::watch::Sender<f32>>,
) -> Result<(), WhisrsError> {
    let decoded = decode_wav(wav_bytes)?;
    play_decoded(decoded, stop, level_tx, None)
}

/// One message from the HTTP body reader to [`play_wav_stream`].
#[derive(Debug)]
pub enum StreamChunk {
    /// The next slice of the response body, split at arbitrary byte offsets.
    Data(Vec<u8>),
    /// The body ended cleanly.
    End,
    /// The body read failed mid-stream.
    Error(String),
}

/// Error for a body reader that hung up without an end marker or `stop`.
const STREAM_ENDED_UNEXPECTEDLY: &str = "TTS audio stream ended unexpectedly";

/// How long [`play_wav_stream`] waits on the channel before re-checking `stop`.
const STREAM_POLL: Duration = Duration::from_millis(20);
/// Give up on a stream that delivers no bytes for this long before its end.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Give up when the sample queue stays full this long: the output device has
/// stopped consuming samples (sink disconnected, ALSA error reported only to
/// cpal's error callback).
const DEVICE_STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Play a WAV response body as it arrives, blocking until playback finishes,
/// `stop` is set, or the stream fails.
///
/// `first_audio` is signalled once, when the first samples are queued to the
/// device (or on the buffered fallback, when playback starts). It is dropped
/// unsignalled if the stream ends, errors or is stopped before any audio.
///
/// The WAV header is parsed incrementally; PCM (8/16/24/32-bit int) and 32-bit
/// float bodies, plain or `WAVE_FORMAT_EXTENSIBLE`, are decoded and played as
/// they arrive. Anything else (not RIFF/WAVE, unsupported format) is buffered
/// to the end of the body and played through [`decode_wav`], as [`play_wav`]
/// does.
///
/// Returns `Ok` on normal completion and on `stop` (including a sender that
/// hangs up because `stop` was set; nothing further is played and
/// `first_audio` is not fired). Returns `Err` on [`StreamChunk::Error`]
/// (queued audio is cut off; the message is passed through as is), on a
/// sender dropped without [`StreamChunk::End`] while not stopped (the reader
/// died), on a stream that stalls for 30 s, on an output device that stops
/// consuming samples for [`DEVICE_STALL_TIMEOUT`] while the queue is full, and
/// on an undecodable buffered body.
///
/// Intended for a blocking task (`spawn_blocking`).
pub fn play_wav_stream(
    chunks: std::sync::mpsc::Receiver<StreamChunk>,
    stop: Arc<AtomicBool>,
    level_tx: Option<tokio::sync::watch::Sender<f32>>,
    first_audio: tokio::sync::oneshot::Sender<()>,
) -> Result<(), WhisrsError> {
    use std::sync::mpsc::RecvTimeoutError;

    enum Outcome {
        Ended,
        Stopped,
        Failed(String),
        Stalled,
        Device(WhisrsError),
    }

    let mut first_audio = Some(first_audio);
    let mut decoder = WavStreamDecoder::new();
    let mut pipeline: Option<StreamPipeline> = None;
    let mut decoded: Vec<f32> = Vec::new();
    let mut body_bytes: u64 = 0;
    let mut last_chunk = Instant::now();

    // A hang-up is an interruption if the reader quit because `stop` was set,
    // and a failure otherwise (the reader sends `End`/`Error` on every other
    // exit, so a bare disconnect means it died, e.g. panicked).
    let disconnect_outcome = |stop: &AtomicBool| {
        if stop.load(Ordering::Acquire) {
            Outcome::Stopped
        } else {
            Outcome::Failed(STREAM_ENDED_UNEXPECTEDLY.to_string())
        }
    };

    let outcome = loop {
        if stop.load(Ordering::Acquire) {
            break Outcome::Stopped;
        }
        match chunks.recv_timeout(STREAM_POLL) {
            Ok(StreamChunk::Data(bytes)) => {
                last_chunk = Instant::now();
                body_bytes += bytes.len() as u64;
                decoded.clear();
                match decoder.push(&bytes, &mut decoded) {
                    PushEvent::HeaderParsed { format, data_len } => {
                        match data_len {
                            Some(n) => debug!(
                                "TTS stream header parsed: {} Hz, {} ch, {}-bit {}, data size {n} bytes",
                                format.sample_rate,
                                format.channels,
                                format.bits,
                                if format.float { "float" } else { "int" },
                            ),
                            None => debug!(
                                "TTS stream header parsed: {} Hz, {} ch, {}-bit {}, data size unknown",
                                format.sample_rate,
                                format.channels,
                                format.bits,
                                if format.float { "float" } else { "int" },
                            ),
                        }
                        pipeline = Some(StreamPipeline::start(format, &stop, level_tx.clone())?);
                    }
                    PushEvent::Fallback(reason) => {
                        debug!("TTS stream not playable incrementally ({reason}); buffering whole body");
                    }
                    PushEvent::None => {}
                }
                if let Some(p) = pipeline.as_mut() {
                    if let Err(e) = p.feed(&decoded, &mut first_audio) {
                        break Outcome::Device(e);
                    }
                    // Feeding waits while the queue is full; that is not a
                    // stalled stream.
                    last_chunk = Instant::now();
                }
            }
            Ok(StreamChunk::End) => break Outcome::Ended,
            Err(RecvTimeoutError::Disconnected) => break disconnect_outcome(&stop),
            Ok(StreamChunk::Error(msg)) => break Outcome::Failed(msg),
            Err(RecvTimeoutError::Timeout) => {
                if last_chunk.elapsed() > STREAM_IDLE_TIMEOUT {
                    break Outcome::Stalled;
                }
            }
        }
    };

    let reset_level = |level_tx: &Option<tokio::sync::watch::Sender<f32>>| {
        if let Some(tx) = level_tx {
            let _ = tx.send(0.0);
        }
    };

    match outcome {
        Outcome::Stopped => {
            debug!("TTS playback interrupted");
            drop(pipeline);
            reset_level(&level_tx);
            Ok(())
        }
        Outcome::Failed(msg) => {
            drop(pipeline);
            reset_level(&level_tx);
            Err(WhisrsError::Audio(msg))
        }
        Outcome::Stalled => {
            drop(pipeline);
            reset_level(&level_tx);
            Err(WhisrsError::Audio(format!(
                "TTS audio stream stalled: no data for {}s",
                STREAM_IDLE_TIMEOUT.as_secs()
            )))
        }
        Outcome::Device(e) => {
            drop(pipeline);
            reset_level(&level_tx);
            Err(e)
        }
        Outcome::Ended => {
            debug!("TTS stream body ended after {body_bytes} bytes");
            match decoder.finish() {
                Some(buffered) => {
                    drop(pipeline);
                    play_buffered_fallback(&buffered, stop, level_tx, first_audio)
                }
                None => {
                    let mut result = Ok(());
                    if let Some(mut p) = pipeline {
                        result = p.finish(&mut first_audio);
                        if result.is_ok() {
                            p.drain(&stop);
                        }
                        drop(p);
                    }
                    reset_level(&level_tx);
                    result
                }
            }
        }
    }
}

/// Play a body the incremental decoder could not handle, once it has fully
/// arrived. When `stop` is set by then (checked after decoding, which can take
/// a moment on a long clip) nothing is played, the device is never opened and
/// a decode failure is not reported: the user already cancelled.
fn play_buffered_fallback(
    buffered: &[u8],
    stop: Arc<AtomicBool>,
    level_tx: Option<tokio::sync::watch::Sender<f32>>,
    first_audio: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<(), WhisrsError> {
    let decoded = decode_wav(buffered);
    if stop.load(Ordering::Acquire) {
        debug!("TTS playback interrupted");
        if let Some(tx) = &level_tx {
            let _ = tx.send(0.0);
        }
        return Ok(());
    }
    play_decoded(decoded?, stop, level_tx, first_audio)
}

// ---------------------------------------------------------------------------
// Incremental WAV parsing (pure, no audio device)
// ---------------------------------------------------------------------------

/// A declared `data` chunk size at or above this is a streaming placeholder
/// (0xFFFFFFFF, 0xFFFFFFDB, 2_000_000_000, ...) rather than a real length.
/// 1 GiB is ~45 min of 48 kHz stereo f32, far longer than any TTS clip.
const UNBOUNDED_DATA_LEN: u64 = 0x4000_0000;

/// `fmt ` chunks larger than this are not a format we understand.
const MAX_FMT_CHUNK_LEN: usize = 1024;

/// The trailing 12 bytes shared by the `KSDATAFORMAT_SUBTYPE_*` GUIDs
/// (`xxxxxxxx-0000-0010-8000-00aa00389b71`); the first 4 carry the format tag.
const KSDATAFORMAT_GUID_TAIL: [u8; 12] = [
    0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

/// How one sample is laid out in the `data` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleKind {
    /// 8-bit unsigned (offset by 128), per the WAV spec and hound.
    U8,
    I16,
    /// 24-bit packed in 3 bytes.
    I24,
    /// 24 valid bits in a 4-byte container.
    I24In32,
    I32,
    F32,
}

/// Stream format from the `fmt ` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WavFormat {
    sample_rate: u32,
    channels: u16,
    /// Valid bits per sample (drives integer normalization, as in [`decode_wav`]).
    bits: u16,
    /// Container bytes per sample (`block_align / channels`).
    bytes_per_sample: u16,
    float: bool,
    kind: SampleKind,
}

impl WavFormat {
    fn block_align(&self) -> usize {
        self.bytes_per_sample as usize * self.channels as usize
    }
}

/// Result of trying to parse a (possibly partial) WAV header.
#[derive(Debug, PartialEq)]
enum HeaderParse {
    /// More bytes are needed to reach the `data` chunk header.
    Incomplete,
    /// Header parsed; the body starts at `data_offset`.
    Parsed {
        format: WavFormat,
        data_offset: usize,
        /// `None` when the declared size is a streaming placeholder.
        data_len: Option<u64>,
    },
    /// Not a stream we can decode incrementally.
    Unsupported(String),
}

fn le_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Parse the header at the start of `buf`, which may be any prefix of the body.
fn parse_wav_header(buf: &[u8]) -> HeaderParse {
    // Reject early on a magic mismatch, even from a short prefix.
    let prefix_ok = |range: std::ops::Range<usize>, want: &[u8; 4]| {
        let end = range.end.min(buf.len());
        range.start >= end || buf[range.start..end] == want[..end - range.start]
    };
    if !prefix_ok(0..4, b"RIFF") || !prefix_ok(8..12, b"WAVE") {
        return HeaderParse::Unsupported("not a RIFF/WAVE stream".to_string());
    }

    let mut format: Option<WavFormat> = None;
    let mut off = 12usize;
    loop {
        if off + 8 > buf.len() {
            return HeaderParse::Incomplete;
        }
        let id = &buf[off..off + 4];
        let len = le_u32(buf, off + 4);
        let payload = off + 8;

        if id == b"data" {
            let Some(format) = format else {
                return HeaderParse::Unsupported("data chunk before fmt chunk".to_string());
            };
            let len = len as u64;
            // 0 is also written by some non-seekable encoders as "unknown".
            let data_len = (len != 0 && len < UNBOUNDED_DATA_LEN).then_some(len);
            return HeaderParse::Parsed {
                format,
                data_offset: payload,
                data_len,
            };
        }

        if len == u32::MAX {
            return HeaderParse::Unsupported(format!(
                "unbounded {:?} chunk before data",
                String::from_utf8_lossy(id)
            ));
        }
        let len = len as usize;

        if id == b"fmt " {
            if len > MAX_FMT_CHUNK_LEN {
                return HeaderParse::Unsupported(format!("oversized fmt chunk ({len} bytes)"));
            }
            if payload + len > buf.len() {
                return HeaderParse::Incomplete;
            }
            match parse_fmt_chunk(&buf[payload..payload + len]) {
                Ok(f) => format = Some(f),
                Err(reason) => return HeaderParse::Unsupported(reason),
            }
        }

        // Chunks are word-aligned: odd-sized payloads carry one pad byte.
        off = payload + len + (len & 1);
    }
}

/// Parse a `fmt ` chunk payload into a supported [`WavFormat`].
fn parse_fmt_chunk(p: &[u8]) -> Result<WavFormat, String> {
    const PCM: u16 = 0x0001;
    const IEEE_FLOAT: u16 = 0x0003;
    const EXTENSIBLE: u16 = 0xfffe;

    if p.len() < 16 {
        return Err(format!("fmt chunk too short ({} bytes)", p.len()));
    }
    let mut tag = le_u16(p, 0);
    let channels = le_u16(p, 2);
    let sample_rate = le_u32(p, 4);
    let block_align = le_u16(p, 12);
    let mut bits = le_u16(p, 14);

    if tag == EXTENSIBLE {
        if p.len() < 40 {
            return Err("WAVE_FORMAT_EXTENSIBLE fmt chunk too short".to_string());
        }
        let valid_bits = le_u16(p, 18);
        let sub = &p[24..40];
        if sub[4..16] != KSDATAFORMAT_GUID_TAIL {
            return Err("unknown WAVE_FORMAT_EXTENSIBLE subformat".to_string());
        }
        let sub_tag = le_u32(sub, 0);
        tag = u16::try_from(sub_tag).map_err(|_| format!("unknown subformat {sub_tag:#x}"))?;
        // Same rule as hound: a zero valid-bits field falls back to the container.
        if valid_bits > 0 {
            bits = valid_bits;
        }
    }

    if channels == 0 {
        return Err("zero channels".to_string());
    }
    if sample_rate == 0 {
        return Err("zero sample rate".to_string());
    }
    if block_align == 0 || !block_align.is_multiple_of(channels) {
        return Err(format!(
            "block align {block_align} does not fit {channels} channels"
        ));
    }
    let bytes_per_sample = block_align / channels;

    let (float, kind) = match (tag, bytes_per_sample, bits) {
        (PCM, 1, 8) => (false, SampleKind::U8),
        (PCM, 2, 16) => (false, SampleKind::I16),
        (PCM, 3, 24) => (false, SampleKind::I24),
        (PCM, 4, 24) => (false, SampleKind::I24In32),
        (PCM, 4, 32) => (false, SampleKind::I32),
        (IEEE_FLOAT, 4, 32) => (true, SampleKind::F32),
        _ => {
            return Err(format!(
                "unsupported format tag {tag:#x} with {bytes_per_sample}-byte / {bits}-bit samples"
            ))
        }
    };

    Ok(WavFormat {
        sample_rate,
        channels,
        bits,
        bytes_per_sample,
        float,
        kind,
    })
}

/// Decodes `data` chunk bytes into f32 samples, carrying partial frames across
/// pushes and honoring a finite declared data size.
#[derive(Debug)]
struct FrameDecoder {
    format: WavFormat,
    /// Bytes still allowed by a finite declared data size; `None` = unbounded.
    remaining: Option<u64>,
    /// Bytes of an incomplete frame from the previous push.
    carry: Vec<u8>,
    /// Integer full-scale value, computed exactly like [`decode_wav`].
    max_amp: f32,
}

impl FrameDecoder {
    fn new(format: WavFormat, data_len: Option<u64>) -> Self {
        let max_amp = if format.float {
            1.0
        } else {
            (1i64 << (format.bits - 1)) as f32
        };
        Self {
            format,
            remaining: data_len,
            carry: Vec::with_capacity(format.block_align()),
            max_amp,
        }
    }

    /// Append the whole frames contained in `carry + bytes` to `out`.
    fn push(&mut self, mut bytes: &[u8], out: &mut Vec<f32>) {
        if let Some(rem) = self.remaining.as_mut() {
            let take = (*rem).min(bytes.len() as u64) as usize;
            bytes = &bytes[..take];
            *rem -= take as u64;
        }
        let ba = self.format.block_align();
        if !self.carry.is_empty() {
            let take = (ba - self.carry.len()).min(bytes.len());
            self.carry.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.carry.len() < ba {
                return;
            }
            let frame = std::mem::take(&mut self.carry);
            self.decode_frames(&frame, out);
            self.carry = frame;
            self.carry.clear();
        }
        let whole = bytes.len() - bytes.len() % ba;
        self.decode_frames(&bytes[..whole], out);
        self.carry.extend_from_slice(&bytes[whole..]);
    }

    fn decode_frames(&self, bytes: &[u8], out: &mut Vec<f32>) {
        let bps = self.format.bytes_per_sample as usize;
        out.reserve(bytes.len() / bps);
        for s in bytes.chunks_exact(bps) {
            let v = match self.format.kind {
                SampleKind::F32 => {
                    out.push(f32::from_le_bytes([s[0], s[1], s[2], s[3]]));
                    continue;
                }
                SampleKind::U8 => s[0] as i32 - 128,
                SampleKind::I16 => i16::from_le_bytes([s[0], s[1]]) as i32,
                // Place the 24 bits at the top of an i32, then arithmetic-shift
                // back down to sign-extend.
                SampleKind::I24 | SampleKind::I24In32 => {
                    i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8
                }
                SampleKind::I32 => i32::from_le_bytes([s[0], s[1], s[2], s[3]]),
            };
            out.push(v as f32 / self.max_amp);
        }
    }
}

/// What changed during one [`WavStreamDecoder::push`].
#[derive(Debug, PartialEq)]
enum PushEvent {
    None,
    /// The header completed on this push; body samples may follow in `out`.
    HeaderParsed {
        format: WavFormat,
        data_len: Option<u64>,
    },
    /// The stream cannot be decoded incrementally; bytes are now buffered.
    Fallback(String),
}

#[derive(Debug)]
enum DecoderPhase {
    Header(Vec<u8>),
    Body(FrameDecoder),
    Fallback(Vec<u8>),
}

/// Incremental WAV body decoder: header, then frames, or whole-body buffering.
#[derive(Debug)]
struct WavStreamDecoder {
    phase: DecoderPhase,
}

impl WavStreamDecoder {
    fn new() -> Self {
        Self {
            phase: DecoderPhase::Header(Vec::with_capacity(128)),
        }
    }

    /// Feed the next body bytes; decoded samples are appended to `out`.
    fn push(&mut self, bytes: &[u8], out: &mut Vec<f32>) -> PushEvent {
        match &mut self.phase {
            DecoderPhase::Header(buf) => {
                buf.extend_from_slice(bytes);
                match parse_wav_header(buf) {
                    HeaderParse::Incomplete => PushEvent::None,
                    HeaderParse::Parsed {
                        format,
                        data_offset,
                        data_len,
                    } => {
                        let body = buf.split_off(data_offset);
                        let mut dec = FrameDecoder::new(format, data_len);
                        dec.push(&body, out);
                        self.phase = DecoderPhase::Body(dec);
                        PushEvent::HeaderParsed { format, data_len }
                    }
                    HeaderParse::Unsupported(reason) => {
                        let buf = std::mem::take(buf);
                        self.phase = DecoderPhase::Fallback(buf);
                        PushEvent::Fallback(reason)
                    }
                }
            }
            DecoderPhase::Body(dec) => {
                dec.push(bytes, out);
                PushEvent::None
            }
            DecoderPhase::Fallback(buf) => {
                buf.extend_from_slice(bytes);
                PushEvent::None
            }
        }
    }

    /// End of body. Returns the buffered bytes when they must go through the
    /// whole-file fallback (unsupported stream, or header never completed).
    /// A trailing partial frame in the incremental path is discarded.
    fn finish(self) -> Option<Vec<u8>> {
        match self.phase {
            DecoderPhase::Header(buf) | DecoderPhase::Fallback(buf) => Some(buf),
            DecoderPhase::Body(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming resampler (pure)
// ---------------------------------------------------------------------------

/// Chunk-by-chunk equivalent of [`resample_remap`]: downmix to mono, linear
/// interpolation, fan out. Output frame `j` sits at source position
/// `j * src_rate / dst_rate`, computed in exact integer arithmetic from the
/// global frame index, so chunk boundaries never restart or drift.
#[derive(Debug)]
struct StreamResampler {
    src_ch: usize,
    dst_ch: usize,
    src_rate: u64,
    dst_rate: u64,
    /// Mono source frames from global index `base` onward, still needed.
    mono: Vec<f32>,
    base: u64,
    /// Source frames seen so far.
    seen: u64,
    /// Next output frame index.
    next_out: u64,
}

impl StreamResampler {
    fn new(src_channels: u16, src_rate: u32, dst_channels: u16, dst_rate: u32) -> Self {
        Self {
            src_ch: src_channels.max(1) as usize,
            dst_ch: dst_channels.max(1) as usize,
            src_rate: src_rate as u64,
            dst_rate: dst_rate as u64,
            mono: Vec::new(),
            base: 0,
            seen: 0,
            next_out: 0,
        }
    }

    fn fan(&self, out: &mut Vec<f32>, v: f32) {
        for _ in 0..self.dst_ch {
            out.push(v);
        }
    }

    fn passthrough_rate(&self) -> bool {
        self.src_rate == self.dst_rate
    }

    /// Source position of output frame `j`: (integer index, fraction).
    fn position(&self, j: u64) -> (u64, f32) {
        let num = j * self.src_rate;
        let i0 = num / self.dst_rate;
        let frac = ((num % self.dst_rate) as f64 / self.dst_rate as f64) as f32;
        (i0, frac)
    }

    /// Feed interleaved source samples (whole frames); append output to `out`.
    fn push(&mut self, src: &[f32], out: &mut Vec<f32>) {
        if self.src_rate == 0 || self.dst_rate == 0 {
            return;
        }
        let frames = src.len() / self.src_ch;
        for frame in src.chunks_exact(self.src_ch) {
            let acc: f32 = frame.iter().sum();
            self.mono.push(acc / self.src_ch as f32);
        }
        self.seen += frames as u64;

        if self.passthrough_rate() {
            let mono = std::mem::take(&mut self.mono);
            out.reserve(mono.len() * self.dst_ch);
            for &v in &mono {
                self.fan(out, v);
            }
            self.mono = mono;
            self.mono.clear();
            self.base = self.seen;
            self.next_out = self.seen;
            return;
        }

        loop {
            let j = self.next_out;
            // Never emit past what the final length would be for `seen` frames.
            if (j + 1) * self.src_rate > self.seen * self.dst_rate {
                break;
            }
            let (i0, frac) = self.position(j);
            // Interpolation needs the frame after i0, not yet arrived.
            if i0 + 1 >= self.seen {
                break;
            }
            let a = self.mono[(i0 - self.base) as usize];
            let b = self.mono[(i0 + 1 - self.base) as usize];
            self.fan(out, a + (b - a) * frac);
            self.next_out += 1;
        }

        // Drop frames no later output can reference; always keep the last one
        // so the end-of-stream clamp has something to read.
        let (next_i0, _) = self.position(self.next_out);
        let keep_from = next_i0.min(self.seen.saturating_sub(1)).max(self.base);
        self.mono.drain(..(keep_from - self.base) as usize);
        self.base = keep_from;
    }

    /// End of stream: emit the remaining frames, clamping at the last source
    /// frame exactly as [`resample_remap`] does.
    fn finish(&mut self, out: &mut Vec<f32>) {
        if self.seen == 0 || self.src_rate == 0 || self.dst_rate == 0 || self.passthrough_rate() {
            return;
        }
        let total = ((self.seen * self.dst_rate) / self.src_rate).max(1);
        let last = self.seen - 1;
        while self.next_out < total {
            let (i0, frac) = self.position(self.next_out);
            let a = self.mono[(i0.min(last) - self.base) as usize];
            let b = self.mono[((i0 + 1).min(last) - self.base) as usize];
            self.fan(out, a + (b - a) * frac);
            self.next_out += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Device output
// ---------------------------------------------------------------------------

/// Open the default output device at its preferred rate/channels, falling back
/// to the clip's native format when the device reports no default config.
fn open_output(
    native_rate: u32,
    native_channels: u16,
) -> Result<(cpal::Device, StreamConfig), WhisrsError> {
    let host = super::device::alsa_host().map_err(|e| WhisrsError::Audio(e.to_string()))?;
    let device = host
        .default_output_device()
        .ok_or_else(|| WhisrsError::Audio("no default audio output device".to_string()))?;

    // Play at the device's preferred rate/channels and resample/remap the clip
    // to match. Forcing the clip's native format (e.g. Groq's 24 kHz mono) can
    // fail to build a stream on devices that only advertise their default rate.
    let (target_rate, target_channels) = match device.default_output_config() {
        Ok(cfg) => (cfg.sample_rate(), cfg.channels().max(1)),
        Err(e) => {
            debug!("no default output config ({e}); using clip's native format");
            (native_rate, native_channels.max(1))
        }
    };

    let config = StreamConfig {
        channels: target_channels,
        sample_rate: target_rate,
        buffer_size: cpal::BufferSize::Default,
    };
    Ok((device, config))
}

/// Device audio held in the sample queue. The queue is allocated once at this
/// size and never grows: the feeder waits for room instead, so neither side
/// ever allocates while holding the lock.
const QUEUE_SECONDS: f64 = 10.0;
/// Device audio copied per lock acquisition, so the feeder only ever holds the
/// lock the audio callback `try_lock`s for a few microseconds.
const ENQUEUE_BATCH_SECONDS: f64 = 0.005;

/// Resampler plus a cpal stream fed from a shared sample queue.
struct StreamPipeline {
    resampler: StreamResampler,
    queue: Arc<Mutex<VecDeque<f32>>>,
    /// Samples copied into `queue` per lock acquisition.
    batch: usize,
    stop: Arc<AtomicBool>,
    /// Device samples per second (rate * channels), for drain bounds.
    samples_per_sec: f64,
    /// Scratch buffer for resampler output.
    scratch: Vec<f32>,
    /// Kept alive for the pipeline's lifetime; dropping it stops the device.
    _stream: cpal::Stream,
}

impl StreamPipeline {
    /// Open the device and start a stream that plays silence until fed.
    fn start(
        format: WavFormat,
        stop: &Arc<AtomicBool>,
        level_tx: Option<tokio::sync::watch::Sender<f32>>,
    ) -> Result<Self, WhisrsError> {
        let (device, config) = open_output(format.sample_rate, format.channels)?;
        let samples_per_sec = config.sample_rate as f64 * config.channels as f64;
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(
            (samples_per_sec * QUEUE_SECONDS) as usize,
        )));

        let queue_cb = Arc::clone(&queue);
        let stop_cb = Arc::clone(stop);
        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    if stop_cb.load(Ordering::Acquire) {
                        data.fill(0.0);
                        if let Some(tx) = &level_tx {
                            let _ = tx.send(0.0);
                        }
                        return;
                    }
                    // Never block the audio thread: a contended or empty queue
                    // is an underrun and plays silence.
                    let mut filled = 0;
                    if let Ok(mut q) = queue_cb.try_lock() {
                        let n = q.len().min(data.len());
                        for (d, s) in data[..n].iter_mut().zip(q.drain(..n)) {
                            *d = s;
                        }
                        filled = n;
                    }
                    data[filled..].fill(0.0);
                    if let Some(tx) = &level_tx {
                        let _ = tx.send(playback_level(data));
                    }
                },
                |err| {
                    warn!("TTS playback stream error: {err}");
                },
                None,
            )
            .map_err(|e| WhisrsError::Audio(format!("failed to build output stream: {e}")))?;
        stream
            .play()
            .map_err(|e| WhisrsError::Audio(format!("failed to start playback: {e}")))?;

        Ok(Self {
            resampler: StreamResampler::new(
                format.channels,
                format.sample_rate,
                config.channels,
                config.sample_rate,
            ),
            queue,
            batch: ((samples_per_sec * ENQUEUE_BATCH_SECONDS) as usize).max(1),
            stop: Arc::clone(stop),
            samples_per_sec,
            scratch: Vec::new(),
            _stream: stream,
        })
    }

    /// Resample decoded source samples and queue them for the device.
    fn feed(
        &mut self,
        decoded: &[f32],
        first_audio: &mut Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<(), WhisrsError> {
        self.scratch.clear();
        self.resampler.push(decoded, &mut self.scratch);
        self.enqueue(first_audio)
    }

    /// Flush the resampler tail at end of stream.
    fn finish(
        &mut self,
        first_audio: &mut Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<(), WhisrsError> {
        self.scratch.clear();
        self.resampler.finish(&mut self.scratch);
        self.enqueue(first_audio)
    }

    fn enqueue(
        &mut self,
        first_audio: &mut Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<(), WhisrsError> {
        let mut first = || {
            if let Some(tx) = first_audio.take() {
                debug!("TTS first audio queued");
                let _ = tx.send(());
            }
        };
        enqueue_bounded(
            &self.queue,
            &self.scratch,
            self.batch,
            &self.stop,
            DEVICE_STALL_TIMEOUT,
            &mut first,
        )
    }

    fn queued(&self) -> usize {
        self.queue.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Block until the queue drains (bounded by its length + 2 s) or `stop`.
    fn drain(&self, stop: &AtomicBool) {
        let bound = Duration::from_secs_f64(self.queued() as f64 / self.samples_per_sec + 2.0);
        let start = Instant::now();
        loop {
            if stop.load(Ordering::Acquire) {
                debug!("TTS playback interrupted");
                return;
            }
            if self.queued() == 0 {
                break;
            }
            if start.elapsed() > bound {
                debug!(
                    "TTS playback drain timed out after {:.1}s",
                    bound.as_secs_f64()
                );
                break;
            }
            std::thread::sleep(STREAM_POLL);
        }
        // Let the device play out its last buffer.
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Append `samples` to the fixed-capacity `queue` without ever growing it,
/// copying at most `batch` samples per lock acquisition and sleeping with the
/// lock released while the queue is full. `on_first_push` runs once, after
/// the first samples land. Returns `Ok` early (dropping the rest) when `stop`
/// is set or the lock is poisoned.
///
/// Returns `Err` when the queue stays full for `stall_timeout` without the
/// consumer taking a single sample: the device has stopped pulling, and
/// waiting on it would hang read-aloud in Speaking until `stop`.
///
/// This keeps the lock the audio callback `try_lock`s short, bounded and
/// allocation-free; a failed `try_lock` there costs a buffer of silence.
fn enqueue_bounded(
    queue: &Mutex<VecDeque<f32>>,
    samples: &[f32],
    batch: usize,
    stop: &AtomicBool,
    stall_timeout: Duration,
    on_first_push: &mut dyn FnMut(),
) -> Result<(), WhisrsError> {
    let mut rest = samples;
    let mut pushed_any = false;
    // Last time room appeared in the queue, i.e. the consumer made progress.
    let mut last_progress = Instant::now();
    while !rest.is_empty() {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let pushed = {
            let Ok(mut q) = queue.lock() else {
                return Ok(());
            };
            let room = q.capacity() - q.len();
            let n = room.min(batch).min(rest.len());
            q.extend(rest[..n].iter().copied());
            n
        };
        if pushed == 0 {
            if last_progress.elapsed() >= stall_timeout {
                return Err(WhisrsError::Audio(
                    "audio output stalled: device stopped consuming samples".to_string(),
                ));
            }
            // Full: seconds of audio are queued, so a short wait is harmless.
            std::thread::sleep(STREAM_POLL.min(stall_timeout));
            continue;
        }
        last_progress = Instant::now();
        if !pushed_any {
            pushed_any = true;
            on_first_push();
        }
        rest = &rest[pushed..];
    }
    Ok(())
}

/// Normalized playback amplitude from a buffer of interleaved f32 samples.
///
/// Mirrors [`crate::audio::capture`]'s level mapping: RMS through a soft
/// compressor `1 - exp(-rms * 18)` so typical speech reaches the upper part
/// of the visualizer's dynamic range.
fn playback_level(data: &[f32]) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    let sum_squares: f32 = data.iter().map(|s| s * s).sum();
    let rms = (sum_squares / data.len() as f32).sqrt();
    (1.0 - (-rms * 18.0).exp()).clamp(0.0, 1.0)
}

/// Resample interleaved f32 audio from `(src_rate, src_channels)` to
/// `(dst_rate, dst_channels)`.
///
/// The source is downmixed to mono and then fanned out across the destination
/// channels, with linear interpolation for the rate conversion. This is aimed
/// at speech (Groq's TTS is mono); a stereo music source would lose its imaging,
/// which is acceptable for this playback path.
fn resample_remap(
    src: &[f32],
    src_channels: u16,
    src_rate: u32,
    dst_channels: u16,
    dst_rate: u32,
) -> Vec<f32> {
    let src_ch = src_channels.max(1) as usize;
    let dst_ch = dst_channels.max(1) as usize;
    let src_frames = src.len() / src_ch;
    if src_frames == 0 || src_rate == 0 || dst_rate == 0 {
        return Vec::new();
    }

    // Downmix to a mono signal.
    let mono: Vec<f32> = (0..src_frames)
        .map(|i| {
            let acc: f32 = (0..src_ch).map(|c| src[i * src_ch + c]).sum();
            acc / src_ch as f32
        })
        .collect();

    // Fan a mono frame value out across all destination channels.
    let fan = |out: &mut Vec<f32>, v: f32| {
        for _ in 0..dst_ch {
            out.push(v);
        }
    };

    if src_rate == dst_rate {
        let mut out = Vec::with_capacity(mono.len() * dst_ch);
        for v in mono {
            fan(&mut out, v);
        }
        return out;
    }

    let dst_frames = ((src_frames as u64 * dst_rate as u64) / src_rate as u64).max(1) as usize;
    let ratio = src_rate as f64 / dst_rate as f64;
    let mut out = Vec::with_capacity(dst_frames * dst_ch);
    for j in 0..dst_frames {
        let pos = j as f64 * ratio;
        let i0 = pos.floor() as usize;
        let frac = (pos - i0 as f64) as f32;
        let a = mono[i0.min(src_frames - 1)];
        let b = mono[(i0 + 1).min(src_frames - 1)];
        fan(&mut out, a + (b - a) * frac);
    }
    out
}

/// Play already-decoded PCM on the default output device. See [`play_wav`].
///
/// `on_start`, when given, is signalled once the stream starts playing.
fn play_decoded(
    decoded: DecodedWav,
    stop: Arc<AtomicBool>,
    level_tx: Option<tokio::sync::watch::Sender<f32>>,
    on_start: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<(), WhisrsError> {
    if decoded.samples.is_empty() {
        return Ok(());
    }

    let (device, config) = open_output(decoded.sample_rate, decoded.channels)?;
    let target_rate = config.sample_rate;
    let target_channels = config.channels;

    let samples = resample_remap(
        &decoded.samples,
        decoded.channels,
        decoded.sample_rate,
        target_channels,
        target_rate,
    );
    if samples.is_empty() {
        return Ok(());
    }
    let samples_len = samples.len();
    let sample_idx = Arc::new(AtomicUsize::new(0));
    let sample_idx_cb = Arc::clone(&sample_idx);
    let done = Arc::new(AtomicBool::new(false));
    let done_cb = Arc::clone(&done);
    let stop_cb = Arc::clone(&stop);

    let level_tx_cb = level_tx.clone();
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                if stop_cb.load(Ordering::Acquire) {
                    for sample in data.iter_mut() {
                        *sample = 0.0;
                    }
                    done_cb.store(true, Ordering::Release);
                    if let Some(tx) = &level_tx_cb {
                        let _ = tx.send(0.0);
                    }
                    return;
                }
                for sample in data.iter_mut() {
                    let idx = sample_idx_cb.fetch_add(1, Ordering::Relaxed);
                    if idx < samples_len {
                        *sample = samples[idx];
                    } else {
                        *sample = 0.0;
                        done_cb.store(true, Ordering::Release);
                    }
                }
                // Publish the amplitude of the buffer we just emitted so a
                // speaking overlay can react. Best-effort; watch coalesces.
                if let Some(tx) = &level_tx_cb {
                    let _ = tx.send(playback_level(data));
                }
            },
            |err| {
                warn!("TTS playback stream error: {err}");
            },
            None,
        )
        .map_err(|e| WhisrsError::Audio(format!("failed to build output stream: {e}")))?;

    stream
        .play()
        .map_err(|e| WhisrsError::Audio(format!("failed to start playback: {e}")))?;
    if let Some(tx) = on_start {
        let _ = tx.send(());
    }

    // Compute a generous upper bound on playback duration so a stuck stream
    // can't block forever.
    let frames = (samples_len / target_channels.max(1) as usize) as f64;
    let clip_secs = frames / target_rate.max(1) as f64;
    let timeout = Duration::from_secs_f64(clip_secs + 2.0);
    let start = Instant::now();

    while !done.load(Ordering::Acquire) {
        if stop.load(Ordering::Acquire) {
            debug!("TTS playback interrupted");
            break;
        }
        if start.elapsed() > timeout {
            debug!("TTS playback timed out after {:.1}s", timeout.as_secs_f64());
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Let the final buffer drain.
    std::thread::sleep(Duration::from_millis(50));
    drop(stream);
    // Reset the visualizer to silence once playback ends.
    if let Some(tx) = &level_tx {
        let _ = tx.send(0.0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a short stereo i16 tone WAV in memory for decode round-trip tests.
    fn make_i16_wav(sample_rate: u32, channels: u16, frames: usize) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut buf, spec).unwrap();
            for i in 0..frames {
                let v = ((i as f32 * 0.01).sin() * i16::MAX as f32) as i16;
                for _ in 0..channels {
                    writer.write_sample(v).unwrap();
                }
            }
            writer.finalize().unwrap();
        }
        buf.into_inner()
    }

    fn make_f32_wav(sample_rate: u32, channels: u16, frames: usize) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut buf, spec).unwrap();
            for i in 0..frames {
                let v = (i as f32 * 0.01).sin() * 0.5;
                for _ in 0..channels {
                    writer.write_sample(v).unwrap();
                }
            }
            writer.finalize().unwrap();
        }
        buf.into_inner()
    }

    /// `decode_wav` reads the format and every sample of an i16 or f32 WAV,
    /// normalized into [-1.0, 1.0].
    #[test]
    fn decode_wav_reads_format_and_samples() {
        for (kind, wav, rate, channels, frames) in [
            ("i16", make_i16_wav(24_000, 2, 100), 24_000, 2, 100),
            ("i16", make_i16_wav(16_000, 1, 50), 16_000, 1, 50),
            ("f32", make_f32_wav(44_100, 1, 64), 44_100, 1, 64),
        ] {
            let row = format!("{kind}, {channels} ch, {rate} Hz");
            let decoded = decode_wav(&wav).unwrap_or_else(|e| panic!("{row}: {e}"));
            assert_eq!(decoded.sample_rate, rate, "{row}");
            assert_eq!(decoded.channels, channels, "{row}");
            assert_eq!(decoded.samples.len(), frames * channels as usize, "{row}");
            assert_eq!(decoded.frames(), frames, "{row}");
            assert!(
                decoded.samples.iter().all(|s| (-1.0..=1.0).contains(s)),
                "{row}: a sample outside [-1.0, 1.0]"
            );
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        let err = decode_wav(b"not a wav file at all").unwrap_err();
        assert!(err.to_string().contains("WAV header"));
    }

    /// A WAV streamed by ffmpeg/Lavf (as Groq returns) leaves the RIFF and data
    /// chunk lengths as the 0xFFFFFFFF sentinel; hound rejects it raw, but
    /// `decode_wav` repairs the lengths first.
    #[test]
    fn decode_repairs_streaming_sentinel_lengths() {
        let mut wav = make_i16_wav(24_000, 1, 100);
        let dpos = wav
            .windows(4)
            .position(|w| w == b"data")
            .expect("data chunk header");
        wav[4..8].copy_from_slice(&u32::MAX.to_le_bytes()); // RIFF size sentinel
        wav[dpos + 4..dpos + 8].copy_from_slice(&u32::MAX.to_le_bytes()); // data size sentinel

        // Raw hound rejects the sentinel data length...
        assert!(hound::WavReader::new(Cursor::new(&wav)).is_err());
        // ...but decode_wav repairs and reads it.
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.sample_rate, 24_000);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.frames(), 100);
    }

    #[test]
    fn resample_noop_when_format_matches() {
        let mono = vec![0.1f32, 0.2, 0.3, 0.4];
        assert_eq!(resample_remap(&mono, 1, 16_000, 1, 16_000), mono);
    }

    #[test]
    fn resample_changes_rate_and_upmixes_channels() {
        // 24 kHz mono -> 48 kHz stereo: frame count doubles, channels double.
        let mono: Vec<f32> = (0..100).map(|i| (i as f32 * 0.1).sin()).collect();
        let out = resample_remap(&mono, 1, 24_000, 2, 48_000);
        assert_eq!(out.len(), 200 * 2);
        // Left/right are identical (mono fanned out).
        assert_eq!(out[0], out[1]);
    }

    #[test]
    fn playback_level_silence_is_zero() {
        assert_eq!(playback_level(&[]), 0.0);
        assert_eq!(playback_level(&[0.0, 0.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn playback_level_loud_is_higher_than_quiet() {
        let quiet = playback_level(&[0.01, -0.01, 0.01, -0.01]);
        let loud = playback_level(&[0.8, -0.8, 0.8, -0.8]);
        assert!(loud > quiet);
        assert!((0.0..=1.0).contains(&loud));
        assert!((0.0..=1.0).contains(&quiet));
    }

    #[test]
    fn resample_downmix_stereo_to_mono() {
        // Interleaved stereo [1.0, -1.0, ...] downmixes to ~0.0 mono, same rate.
        let stereo = vec![1.0f32, -1.0, 1.0, -1.0];
        let out = resample_remap(&stereo, 2, 16_000, 1, 16_000);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|s| s.abs() < 1e-6));
    }

    // -----------------------------------------------------------------
    // Incremental playback: parser, frame decoder, resampler
    // -----------------------------------------------------------------

    /// Tiny deterministic PRNG so chunk splits are reproducible without `rand`.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        /// A chunk length in `1..=max`.
        fn len(&mut self, max: usize) -> usize {
            1 + (self.next() as usize % max)
        }
    }

    /// Split `bytes` into random-size chunks (1..=max bytes).
    fn random_chunks(bytes: &[u8], seed: u64, max: usize) -> Vec<&[u8]> {
        let mut rng = Lcg(seed);
        let mut out = Vec::new();
        let mut off = 0;
        while off < bytes.len() {
            let n = rng.len(max).min(bytes.len() - off);
            out.push(&bytes[off..off + n]);
            off += n;
        }
        out
    }

    /// Feed `chunks` through a [`WavStreamDecoder`]; return samples, the
    /// parsed format (if any), and whether it fell back.
    fn stream_decode(chunks: &[&[u8]]) -> (Vec<f32>, Option<WavFormat>, Option<Vec<u8>>) {
        let mut dec = WavStreamDecoder::new();
        let mut out = Vec::new();
        let mut format = None;
        for c in chunks {
            if let PushEvent::HeaderParsed { format: f, .. } = dec.push(c, &mut out) {
                assert!(format.is_none(), "header reported twice");
                format = Some(f);
            }
        }
        (out, format, dec.finish())
    }

    /// Build a WAV header by hand: RIFF/WAVE, `fmt `, optional extra chunks,
    /// then a `data` chunk header declaring `data_len`.
    fn build_header(
        fmt: &[u8],
        extra_chunks: &[(&[u8; 4], &[u8])],
        riff_len: u32,
        data_len: u32,
    ) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&riff_len.to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        h.extend_from_slice(fmt);
        if fmt.len() % 2 == 1 {
            h.push(0);
        }
        for (id, payload) in extra_chunks {
            h.extend_from_slice(*id);
            h.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            h.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                h.push(0);
            }
        }
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_len.to_le_bytes());
        h
    }

    /// Plain 16-byte PCM/float `fmt ` payload.
    fn fmt_payload(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let block_align = channels * bits.div_ceil(8);
        let mut p = Vec::new();
        p.extend_from_slice(&tag.to_le_bytes());
        p.extend_from_slice(&channels.to_le_bytes());
        p.extend_from_slice(&rate.to_le_bytes());
        p.extend_from_slice(&(rate * block_align as u32).to_le_bytes());
        p.extend_from_slice(&block_align.to_le_bytes());
        p.extend_from_slice(&bits.to_le_bytes());
        p
    }

    /// ffmpeg's 26-byte LIST/INFO chunk ("ISFT" + "Lavf61.7.100\0" + pad).
    fn lavf_list_chunk() -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(b"INFO");
        p.extend_from_slice(b"ISFT");
        p.extend_from_slice(&13u32.to_le_bytes());
        p.extend_from_slice(b"Lavf61.7.100\0");
        p.push(0); // odd sub-chunk pad
        assert_eq!(p.len(), 26);
        p
    }

    /// Parse `header` fed one split at every offset; all must agree. `row`
    /// names the case in a failure.
    fn assert_parses_at_every_split(row: &str, header: &[u8]) -> (WavFormat, Option<u64>) {
        let whole = match parse_wav_header(header) {
            HeaderParse::Parsed {
                format,
                data_offset,
                data_len,
            } => {
                assert_eq!(data_offset, header.len(), "{row}: data offset");
                (format, data_len)
            }
            other => panic!("{row}: whole header did not parse: {other:?}"),
        };
        for split in 0..=header.len() {
            let mut dec = WavStreamDecoder::new();
            let mut out = Vec::new();
            let mut parsed = None;
            for part in [&header[..split], &header[split..]] {
                match dec.push(part, &mut out) {
                    PushEvent::HeaderParsed { format, data_len } => {
                        parsed = Some((format, data_len))
                    }
                    PushEvent::Fallback(r) => panic!("{row}: split {split}: fell back: {r}"),
                    PushEvent::None => {}
                }
            }
            assert_eq!(parsed, Some(whole), "{row}: split at {split}");
            assert!(out.is_empty(), "{row}: split at {split}: samples out");
            // A strict prefix is never enough.
            if split < header.len() {
                assert_eq!(
                    parse_wav_header(&header[..split]),
                    HeaderParse::Incomplete,
                    "{row}: prefix of {split} bytes"
                );
            }
        }
        whole
    }

    /// A header parses the same at every split, and no strict prefix of it
    /// does: a clean 44-byte one, ffmpeg's with a LIST chunk and sentinel
    /// sizes, and one with an odd-sized chunk and its pad byte.
    #[test]
    fn stream_header_parses_at_every_split() {
        let clean = make_i16_wav(24_000, 1, 10);
        let ffmpeg = build_header(
            &fmt_payload(1, 1, 24_000, 16),
            &[(b"LIST", &lavf_list_chunk())],
            u32::MAX,
            u32::MAX,
        );
        assert_eq!(ffmpeg.len(), 44 + 8 + 26, "the ffmpeg header's length");
        let odd = build_header(
            &fmt_payload(1, 2, 48_000, 16),
            &[(b"junk", &[1, 2, 3])],
            0,
            0x1000,
        );
        for (row, header, rate, channels, data_len) in [
            ("clean 44-byte header", &clean[..44], 24_000, 1, Some(20)),
            ("ffmpeg LIST, sentinels", &ffmpeg[..], 24_000, 1, None),
            ("odd-sized chunk padded", &odd[..], 48_000, 2, Some(0x1000)),
        ] {
            let (format, parsed_len) = assert_parses_at_every_split(row, header);
            assert_eq!(format.sample_rate, rate, "{row}");
            assert_eq!(format.channels, channels, "{row}");
            assert_eq!(format.bits, 16, "{row}");
            assert_eq!(format.kind, SampleKind::I16, "{row}");
            assert_eq!(parsed_len, data_len, "{row}");
        }
    }

    #[test]
    fn stream_header_placeholder_sizes_are_unbounded() {
        let fmt = fmt_payload(1, 1, 24_000, 16);
        for size in [0xFFFF_FFFFu32, 0xFFFF_FFDB, 2_000_000_000, 0] {
            let header = build_header(&fmt, &[], size, size);
            match parse_wav_header(&header) {
                HeaderParse::Parsed { data_len, .. } => {
                    assert_eq!(data_len, None, "size {size:#x}")
                }
                other => panic!("size {size:#x}: {other:?}"),
            }
        }
    }

    #[test]
    fn stream_finite_data_size_truncates_trailing_bytes() {
        let wav = make_i16_wav(16_000, 1, 50);
        let expected = decode_wav(&wav).unwrap().samples;
        let mut padded = wav.clone();
        padded.extend_from_slice(&[0x7f; 64]); // trailing junk past the declared size
        let (samples, format, fallback) = stream_decode(&random_chunks(&padded, 7, 13));
        assert!(format.is_some());
        assert!(fallback.is_none());
        assert_eq!(samples, expected);
    }

    #[test]
    fn stream_non_riff_selects_fallback() {
        for body in [
            &b"ID3\x04 an mp3 would start like this"[..],
            &b"RIFX...."[..],
            &b"RIFF\x00\x00\x00\x00AVI LIST"[..],
            &b"{\"error\": \"nope\"}"[..],
            // One byte is enough to tell.
            &b"{"[..],
        ] {
            let mut dec = WavStreamDecoder::new();
            let mut out = Vec::new();
            let ev = dec.push(body, &mut out);
            assert!(matches!(ev, PushEvent::Fallback(_)), "{body:?}: {ev:?}");
            assert_eq!(dec.finish().as_deref(), Some(body), "{body:?}");
        }
    }

    #[test]
    fn stream_unsupported_format_selects_fallback() {
        // A-law (tag 6) is valid WAV but not decoded incrementally.
        let header = build_header(&fmt_payload(6, 1, 8_000, 8), &[], 0, 100);
        let (_, format, fallback) = stream_decode(&[&header, &[0u8; 100]]);
        assert!(format.is_none());
        assert_eq!(fallback.map(|b| b.len()), Some(header.len() + 100));
    }

    #[test]
    fn stream_truncated_header_falls_back_at_end() {
        let wav = make_i16_wav(16_000, 1, 10);
        let (samples, format, fallback) = stream_decode(&[&wav[..30]]);
        assert!(samples.is_empty());
        assert!(format.is_none());
        assert_eq!(fallback.as_deref(), Some(&wav[..30]));
    }

    /// Stream `wav` in random chunks, at five seeds and chunk sizes, and
    /// check it decodes to what `decode_wav` gives. `row` names the case in
    /// a failure.
    fn assert_stream_matches_decode_wav(row: &str, wav: &[u8]) {
        let expected = decode_wav(wav).unwrap_or_else(|e| panic!("{row}: {e}"));
        for (seed, max) in [(1, 1), (2, 3), (3, 17), (4, 256), (5, 4096)] {
            let (samples, format, fallback) = stream_decode(&random_chunks(wav, seed, max));
            let format = format.unwrap_or_else(|| panic!("{row}: seed {seed}: header not parsed"));
            assert!(fallback.is_none(), "{row}: seed {seed}: fell back");
            assert_eq!(format.sample_rate, expected.sample_rate, "{row}");
            assert_eq!(format.channels, expected.channels, "{row}");
            assert_eq!(
                samples, expected.samples,
                "{row}: seed {seed}, max chunk {max}"
            );
        }
    }

    /// A stereo 22.05 kHz int WAV of `bits` bits per sample. hound writes
    /// >16-bit or >2-channel files as WAVE_FORMAT_EXTENSIBLE.
    fn make_int_wav(bits: u16) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 22_050,
            bits_per_sample: bits,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = hound::WavWriter::new(&mut buf, spec).unwrap();
            let max = ((1i64 << (bits - 1)) - 1) as f32;
            for i in 0..300 {
                let v = ((i as f32 * 0.03).sin() * max) as i32;
                w.write_sample(v).unwrap();
                w.write_sample(-v).unwrap();
            }
            w.finalize().unwrap();
        }
        buf.into_inner()
    }

    /// The streaming decoder gives what `decode_wav` gives, however the
    /// body is chunked.
    #[test]
    fn stream_decode_matches_decode_wav() {
        // ffmpeg layout: fmt, LIST/INFO, data with 0xFFFFFFFF sizes. Only
        // `data` carries the sentinel, so decode_wav's repair walks past
        // LIST; both paths must agree.
        let clean = make_i16_wav(24_000, 1, 400);
        let mut ffmpeg = build_header(
            &fmt_payload(1, 1, 24_000, 16),
            &[(b"LIST", &lavf_list_chunk())],
            u32::MAX,
            u32::MAX,
        );
        ffmpeg.extend_from_slice(&clean[44..]);
        for (row, wav) in [
            ("i16 mono", make_i16_wav(24_000, 1, 1000)),
            ("i16 stereo", make_i16_wav(48_000, 2, 777)),
            ("f32 mono", make_f32_wav(44_100, 1, 500)),
            ("f32 stereo", make_f32_wav(44_100, 2, 333)),
            ("int 8-bit", make_int_wav(8)),
            ("int 24-bit (extensible)", make_int_wav(24)),
            ("int 32-bit (extensible)", make_int_wav(32)),
            ("ffmpeg LIST chunk, sentinel sizes", ffmpeg),
        ] {
            assert_stream_matches_decode_wav(row, &wav);
        }
    }

    #[test]
    fn stream_decode_drops_trailing_partial_frame() {
        let mut wav = make_i16_wav(16_000, 2, 10);
        let dpos = wav.windows(4).position(|w| w == b"data").unwrap();
        wav[dpos + 4..dpos + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        wav.extend_from_slice(&[1, 2, 3]); // 3 bytes of a 4-byte stereo frame
        let (samples, _, _) = stream_decode(&random_chunks(&wav, 9, 5));
        assert_eq!(samples.len(), 20);
    }

    /// Resample `src` in uneven chunks and check it against
    /// `resample_remap`; a failure names the row by its values.
    fn assert_resampler_matches(src: &[f32], sc: u16, sr: u32, dc: u16, dr: u32, seed: u64) {
        let expected = resample_remap(src, sc, sr, dc, dr);
        let mut rs = StreamResampler::new(sc, sr, dc, dr);
        let mut out = Vec::new();
        let mut rng = Lcg(seed);
        let frames = src.len() / sc as usize;
        let row = format!("{frames} frames, {sc} ch {sr} Hz to {dc} ch {dr} Hz, seed {seed}");
        let mut f = 0;
        while f < frames {
            // Uneven chunks, including empty and single-frame ones.
            let n = (rng.next() as usize % 40).min(frames - f);
            rs.push(&src[f * sc as usize..(f + n) * sc as usize], &mut out);
            f += n;
        }
        rs.finish(&mut out);
        let dc = dc as usize;
        let len_diff = (out.len() as isize - expected.len() as isize).unsigned_abs();
        assert!(len_diff <= dc, "{row}: {} vs {}", out.len(), expected.len());
        for (i, (a, b)) in out.iter().zip(&expected).enumerate() {
            assert!((a - b).abs() < 1e-4, "{row}: sample {i}: {a} vs {b}");
        }
        // Bounded memory: at most one output step of history (plus the
        // interpolation neighbour) is retained.
        let bound = (sr as usize).div_ceil(dr as usize) + 2;
        assert!(
            rs.mono.len() <= bound,
            "{row}: retained {} frames",
            rs.mono.len()
        );
    }

    fn tone(frames: usize, channels: u16) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| (0..channels).map(move |c| ((i as f32 * 0.05) + c as f32).sin() * 0.8))
            .collect()
    }

    /// The streaming resampler matches `resample_remap`, fed in uneven
    /// chunks, at every rate and channel change, down to a 3-frame and a
    /// 1-frame input.
    #[test]
    fn stream_resampler_matches_resample_remap() {
        for (frames, sc, sr, dc, dr, seeds) in [
            (2_401, 1, 24_000, 2, 48_000, 1..6),
            (4_801, 2, 48_000, 2, 44_100, 1..6),
            (1_000, 1, 22_050, 2, 48_000, 11..12),
            (1_000, 2, 16_000, 1, 16_000, 12..13),
            (3, 1, 48_000, 2, 8_000, 13..14),
            (1, 1, 24_000, 2, 48_000, 14..15),
        ] {
            let src = tone(frames, sc);
            for seed in seeds {
                assert_resampler_matches(&src, sc, sr, dc, dr, seed);
            }
        }
    }

    #[test]
    fn stream_resampler_empty_input_emits_nothing() {
        let mut rs = StreamResampler::new(1, 24_000, 2, 48_000);
        let mut out = Vec::new();
        rs.push(&[], &mut out);
        rs.finish(&mut out);
        assert!(out.is_empty());
    }

    // --- bounded enqueue --------------------------------------------------

    /// Stall timeout for tests that must never hit it.
    const TEST_STALL: Duration = Duration::from_secs(60);

    #[test]
    fn enqueue_bounded_never_grows_the_queue_and_keeps_order() {
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(64)));
        let cap = queue.lock().unwrap().capacity();
        let stop = AtomicBool::new(false);
        let samples: Vec<f32> = (0..1000).map(|i| i as f32).collect();

        // A consumer standing in for the audio callback.
        let consumer = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                let mut got = Vec::new();
                while got.len() < 1000 {
                    if let Ok(mut q) = queue.try_lock() {
                        assert_eq!(q.capacity(), cap, "queue reallocated");
                        let n = q.len().min(40);
                        got.extend(q.drain(..n));
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                got
            })
        };
        let mut firsts = 0;
        enqueue_bounded(&queue, &samples, 16, &stop, TEST_STALL, &mut || firsts += 1).unwrap();
        assert_eq!(consumer.join().unwrap(), samples);
        assert_eq!(firsts, 1);
        assert_eq!(queue.lock().unwrap().capacity(), cap);
    }

    #[test]
    fn enqueue_bounded_returns_on_stop_while_full() {
        let queue = Mutex::new(VecDeque::<f32>::with_capacity(8));
        let cap = queue.lock().unwrap().capacity();
        let stop = AtomicBool::new(false);
        let mut firsts = 0;
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(60));
                stop.store(true, Ordering::Release);
            });
            // Nothing drains the queue, so this must be ended by `stop`.
            let r = enqueue_bounded(
                &queue,
                &vec![0.5; cap * 4],
                4,
                &stop,
                TEST_STALL,
                &mut || firsts += 1,
            );
            assert!(r.is_ok(), "stop is not a stall: {r:?}");
        });
        assert_eq!(queue.lock().unwrap().len(), cap);
        assert_eq!(firsts, 1);
    }

    #[test]
    fn enqueue_bounded_does_nothing_once_stopped() {
        let queue = Mutex::new(VecDeque::<f32>::with_capacity(8));
        let stop = AtomicBool::new(true);
        let mut firsts = 0;
        enqueue_bounded(&queue, &[0.1; 4], 4, &stop, TEST_STALL, &mut || firsts += 1).unwrap();
        assert!(queue.lock().unwrap().is_empty());
        assert_eq!(firsts, 0);
    }

    #[test]
    fn enqueue_bounded_gives_up_when_nothing_drains() {
        let queue = Mutex::new(VecDeque::<f32>::with_capacity(8));
        let cap = queue.lock().unwrap().capacity();
        let stop = AtomicBool::new(false);
        let mut firsts = 0;
        let start = Instant::now();
        let result = enqueue_bounded(
            &queue,
            &vec![0.5; cap * 4],
            4,
            &stop,
            Duration::from_millis(100),
            &mut || firsts += 1,
        );
        let elapsed = start.elapsed();
        match result {
            Err(WhisrsError::Audio(msg)) => assert_eq!(
                msg,
                "audio output stalled: device stopped consuming samples"
            ),
            other => panic!("expected a stall error, got {other:?}"),
        }
        assert!(
            elapsed >= Duration::from_millis(100),
            "gave up early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(900),
            "gave up late: {elapsed:?}"
        );
        assert_eq!(queue.lock().unwrap().len(), cap);
        assert_eq!(firsts, 1);
    }

    #[test]
    fn enqueue_bounded_slow_draining_is_not_a_stall() {
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(8)));
        let cap = queue.lock().unwrap().capacity();
        let stop = AtomicBool::new(false);
        let total = cap * 5;
        let samples: Vec<f32> = (0..total).map(|i| i as f32).collect();
        let stall = Duration::from_millis(150);

        // Takes a few samples every 50 ms: well under `stall` per step, but
        // the whole enqueue takes several multiples of it.
        let consumer = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                let mut got = Vec::new();
                while got.len() < total {
                    std::thread::sleep(Duration::from_millis(50));
                    let mut q = queue.lock().unwrap();
                    let n = q.len().min(4);
                    got.extend(q.drain(..n));
                }
                got
            })
        };
        let start = Instant::now();
        let mut firsts = 0;
        let result = enqueue_bounded(&queue, &samples, 4, &stop, stall, &mut || firsts += 1);
        let enqueue_time = start.elapsed();
        assert!(
            result.is_ok(),
            "slow draining tripped the stall guard: {result:?}"
        );
        assert!(
            enqueue_time > stall * 2,
            "test did not exercise a long enqueue: {enqueue_time:?}"
        );
        assert_eq!(consumer.join().unwrap(), samples);
        assert_eq!(firsts, 1);
    }

    // --- stream stop / disconnect classification (no device opened) -------

    fn run_stream(chunks: Vec<StreamChunk>, stop: bool) -> (Result<(), WhisrsError>, bool) {
        let (tx, rx) = std::sync::mpsc::channel();
        for c in chunks {
            tx.send(c).unwrap();
        }
        drop(tx);
        let (first_tx, mut first_rx) = tokio::sync::oneshot::channel();
        let result = play_wav_stream(rx, Arc::new(AtomicBool::new(stop)), None, first_tx);
        let fired = first_rx.try_recv().is_ok();
        (result, fired)
    }

    #[test]
    fn stream_stopped_before_first_chunk_plays_nothing() {
        let (result, fired) = run_stream(
            vec![StreamChunk::Data(b"not a wav".to_vec()), StreamChunk::End],
            true,
        );
        assert!(result.is_ok());
        assert!(!fired, "first_audio must not fire on stop");
    }

    /// `play_buffered_fallback` with `stop` set; `stop` is checked before the
    /// device would be opened, so no audio device is touched.
    fn run_fallback_stopped(body: &[u8]) -> (Result<(), WhisrsError>, bool, f32) {
        let (first_tx, mut first_rx) = tokio::sync::oneshot::channel();
        let (level_tx, level_rx) = tokio::sync::watch::channel(0.7f32);
        let result = play_buffered_fallback(
            body,
            Arc::new(AtomicBool::new(true)),
            Some(level_tx),
            Some(first_tx),
        );
        let fired = first_rx.try_recv().is_ok();
        let level = *level_rx.borrow();
        (result, fired, level)
    }

    #[test]
    fn buffered_fallback_stopped_after_end_plays_nothing() {
        // Stop set once the body has fully arrived: a decodable body is
        // dropped without opening the device.
        let (result, fired, level) = run_fallback_stopped(&make_i16_wav(24_000, 1, 2_400));
        assert!(result.is_ok(), "{result:?}");
        assert!(!fired, "first_audio must not fire on stop");
        assert_eq!(level, 0.0, "level must be reset on stop");
    }

    #[test]
    fn buffered_fallback_stopped_suppresses_decode_error() {
        let (result, fired, _) = run_fallback_stopped(b"not a wav");
        assert!(
            result.is_ok(),
            "a cancelled read must not report {result:?}"
        );
        assert!(!fired);
    }

    #[test]
    fn buffered_fallback_undecodable_body_is_an_error_when_not_stopped() {
        let (first_tx, mut first_rx) = tokio::sync::oneshot::channel();
        let result = play_buffered_fallback(
            b"not a wav",
            Arc::new(AtomicBool::new(false)),
            None,
            Some(first_tx),
        );
        assert!(matches!(result, Err(WhisrsError::Audio(_))), "{result:?}");
        assert!(first_rx.try_recv().is_err());
    }

    #[test]
    fn stream_hangup_without_end_or_stop_is_an_error() {
        let (result, fired) = run_stream(vec![StreamChunk::Data(b"not a wav".to_vec())], false);
        match result {
            Err(WhisrsError::Audio(msg)) => assert_eq!(msg, STREAM_ENDED_UNEXPECTEDLY),
            other => panic!("expected an unexpected-end error, got {other:?}"),
        }
        assert!(!fired);
    }

    #[test]
    fn stream_error_message_passes_through_unwrapped() {
        let (result, _) = run_stream(
            vec![StreamChunk::Error("TTS read body failed: reset".into())],
            false,
        );
        match result {
            Err(WhisrsError::Audio(msg)) => assert_eq!(msg, "TTS read body failed: reset"),
            other => panic!("expected the body error, got {other:?}"),
        }
    }
}
