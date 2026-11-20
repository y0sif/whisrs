//! FLAC uploads for recording chunks.
//!
//! Recording chunks reach the backends as 16 kHz mono i16 WAV. FLAC is
//! lossless, so a cloud service decodes the exact samples the WAV holds (same
//! transcript, same billed duration) from, for speech, usually less than half
//! the bytes. Only the native-timestamp `transcribe_segments` of Deepgram,
//! Groq and OpenAI send it: dictation, the phrase fallback, asr-sidecar and
//! local-whisper keep WAV.
//!
//! The encoder (`flacenc`) comes with the `recording` feature. A build
//! without it has no recordings, so no chunk is ever sent, and
//! [`recording_upload`] would hand back the WAV as it is.

use std::borrow::Cow;

#[cfg(feature = "recording")]
use flacenc::bitsink::ByteSink;
#[cfg(feature = "recording")]
use flacenc::component::{BitRepr, Stream};
#[cfg(feature = "recording")]
use flacenc::error::{SourceError, Verify};
#[cfg(feature = "recording")]
use flacenc::source::{Context, Fill, FrameBuf, Source};
#[cfg(feature = "recording")]
use tracing::{debug, warn};

#[cfg(feature = "recording")]
use super::SEGMENT_SAMPLE_RATE;

/// Channels of a recording chunk.
#[cfg(feature = "recording")]
const CHANNELS: usize = 1;

/// Bits per sample of a recording chunk.
#[cfg(feature = "recording")]
const BITS_PER_SAMPLE: usize = 16;

/// Bytes per sample in the WAV data chunk.
#[cfg(feature = "recording")]
const BYTES_PER_SAMPLE: usize = BITS_PER_SAMPLE / 8;

/// How an upload is labelled on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UploadFormat {
    /// `Content-Type` of a raw body (Deepgram) or of the multipart file part.
    pub mime: &'static str,
    /// File name of the multipart file part.
    pub file_name: &'static str,
}

impl UploadFormat {
    pub(crate) const WAV: Self = Self {
        mime: "audio/wav",
        file_name: "audio.wav",
    };
    #[cfg(feature = "recording")]
    pub(crate) const FLAC: Self = Self {
        mime: "audio/flac",
        file_name: "audio.flac",
    };
}

/// Audio ready to send, with the format it is in.
pub(crate) struct Upload<'a> {
    pub bytes: Cow<'a, [u8]>,
    pub format: UploadFormat,
}

/// A recording chunk as it goes on the wire: FLAC, or the WAV unchanged when
/// encoding fails for any reason. A recording never fails over compression.
///
/// The encoder runs on the blocking pool, so a one-hour Deepgram chunk does
/// not stall the daemon's runtime. Empty audio is passed through for the
/// backend's own check to reject.
#[cfg(feature = "recording")]
pub(crate) async fn recording_upload(wav: &[u8]) -> Upload<'_> {
    let unchanged = Upload {
        bytes: Cow::Borrowed(wav),
        format: UploadFormat::WAV,
    };
    if wav.is_empty() {
        return unchanged;
    }

    let owned = wav.to_vec();
    let started = std::time::Instant::now();
    let encoded = tokio::task::spawn_blocking(move || wav_to_flac(&owned)).await;

    match encoded {
        Ok(Ok(flac)) => {
            debug!(
                "encoded {} bytes of WAV as {} bytes of FLAC in {:.2?}",
                wav.len(),
                flac.len(),
                started.elapsed()
            );
            Upload {
                bytes: Cow::Owned(flac),
                format: UploadFormat::FLAC,
            }
        }
        Ok(Err(e)) => {
            warn!("FLAC encoding failed, uploading WAV instead: {e:#}");
            unchanged
        }
        Err(e) => {
            warn!("FLAC encoder did not finish, uploading WAV instead: {e}");
            unchanged
        }
    }
}

/// Without the `recording` feature there is no encoder: the WAV, unchanged.
/// Nothing calls it in such a build, since it records nothing.
#[cfg(not(feature = "recording"))]
pub(crate) async fn recording_upload(wav: &[u8]) -> Upload<'_> {
    Upload {
        bytes: Cow::Borrowed(wav),
        format: UploadFormat::WAV,
    }
}

/// The PCM payload of a 16 kHz mono 16-bit WAV, handed to the encoder one
/// block at a time straight from the WAV bytes, so no decoded copy of the
/// samples is ever held.
#[cfg(feature = "recording")]
struct PcmSource<'a> {
    /// Little-endian samples not read yet.
    pcm: &'a [u8],
}

#[cfg(feature = "recording")]
impl<'a> PcmSource<'a> {
    /// Find the data chunk of `wav`. A WAV in any other format is an error,
    /// which sends the WAV as it is.
    fn new(wav: &'a [u8]) -> anyhow::Result<Self> {
        let reader = hound::WavReader::new(std::io::Cursor::new(wav))?;
        let spec = reader.spec();
        anyhow::ensure!(
            spec.sample_rate as usize == SEGMENT_SAMPLE_RATE
                && spec.channels as usize == CHANNELS
                && spec.bits_per_sample as usize == BITS_PER_SAMPLE
                && spec.sample_format == hound::SampleFormat::Int,
            "not a 16 kHz mono 16-bit PCM WAV: {spec:?}"
        );
        let len = reader.len() as usize * BYTES_PER_SAMPLE;
        // hound stops right after the data chunk's header, whose last four
        // bytes are the chunk size. Matching it rules out 16-bit samples
        // stored in wider slots.
        let start = reader.into_inner().position() as usize;
        let declared = wav
            .get(start.saturating_sub(4)..start)
            .and_then(|size| size.try_into().ok())
            .map(u32::from_le_bytes);
        anyhow::ensure!(
            declared == Some(len as u32),
            "WAV samples are not 2 bytes wide"
        );
        let pcm = wav
            .get(start..start + len)
            .ok_or_else(|| anyhow::anyhow!("WAV data chunk runs past the end of the file"))?;
        Ok(Self { pcm })
    }
}

#[cfg(feature = "recording")]
impl Source for PcmSource<'_> {
    fn channels(&self) -> usize {
        CHANNELS
    }

    fn bits_per_sample(&self) -> usize {
        BITS_PER_SAMPLE
    }

    fn sample_rate(&self) -> usize {
        SEGMENT_SAMPLE_RATE
    }

    fn read_samples<F: Fill>(
        &mut self,
        block_size: usize,
        dest: &mut F,
    ) -> Result<usize, SourceError> {
        let take = (block_size * BYTES_PER_SAMPLE).min(self.pcm.len());
        let (block, rest) = self.pcm.split_at(take);
        dest.fill_le_bytes(block, BYTES_PER_SAMPLE)?;
        self.pcm = rest;
        Ok(block.len() / BYTES_PER_SAMPLE)
    }
}

/// Encode a 16 kHz mono 16-bit WAV as a FLAC stream with flacenc's defaults.
///
/// Frame by frame, each written out as soon as it is encoded. flacenc's
/// one-call `encode_with_fixed_block_size` keeps every frame's residuals
/// (8 bytes per sample) until the end, which is over 400 MB for an hour of
/// audio. The frames are byte-identical to its output; only the STREAMINFO
/// minimum block size differs (see below). Besides `wav`, memory is the FLAC
/// output and one block.
#[cfg(feature = "recording")]
pub(crate) fn wav_to_flac(wav: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut source = PcmSource::new(wav)?;
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| flac_error(e))?;
    let block_size = config.block_size;

    let mut stream =
        Stream::new(SEGMENT_SAMPLE_RATE, CHANNELS, BITS_PER_SAMPLE).map_err(flac_error)?;
    stream
        .stream_info_mut()
        .set_block_sizes(block_size, block_size)
        .map_err(flac_error)?;

    // Room for 8 bits a sample (a real recording took 6.8), so this rarely
    // regrows.
    let mut out = ByteSink::with_capacity(source.pcm.len() * 4);
    // A placeholder header, overwritten once the MD5 and length are known.
    // The stream holds no frames, so this writes only `fLaC` and STREAMINFO.
    stream.write(&mut out).map_err(flac_error)?;
    let header_len = out.as_slice().len();

    let mut input = (
        FrameBuf::with_size(CHANNELS, block_size).map_err(flac_error)?,
        Context::new(BITS_PER_SAMPLE, CHANNELS),
    );
    loop {
        let read = source
            .read_samples(block_size, &mut input)
            .map_err(flac_error)?;
        if read == 0 {
            break;
        }
        let (frame_buf, context) = &input;
        let frame_number = context
            .current_frame_number()
            .ok_or_else(|| anyhow::anyhow!("FLAC encoder: no frame was read"))?;
        let frame = flacenc::encode_fixed_size_frame(
            &config,
            frame_buf,
            frame_number,
            stream.stream_info(),
        )
        .map_err(flac_error)?;
        stream.stream_info_mut().update_frame_info(&frame);
        frame.write(&mut out).map_err(flac_error)?;
    }

    let context = &input.1;
    let info = stream.stream_info_mut();
    info.set_md5_digest(&context.md5_digest());
    info.set_total_samples(context.total_samples());
    // flacenc lowers the minimum to the short last block, which libFLAC
    // reads as a variable block size (and warns) and claxon rejects below
    // 16. The reference encoder writes the fixed size for both.
    info.set_block_sizes(block_size, block_size)
        .map_err(flac_error)?;

    let mut header = ByteSink::new();
    stream.write(&mut header).map_err(flac_error)?;
    let header = header.into_inner();
    anyhow::ensure!(
        header.len() == header_len,
        "FLAC encoder: header changed size"
    );
    let mut out = out.into_inner();
    out[..header_len].copy_from_slice(&header);
    Ok(out)
}

/// flacenc's errors are not `Send` (some hold an `Rc`), so keep the message.
#[cfg(feature = "recording")]
fn flac_error(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("FLAC encoder: {e}")
}

#[cfg(all(test, feature = "recording"))]
mod tests {
    use super::*;
    use crate::audio::wav::encode_wav;

    /// FLAC of `samples`, through the WAV a recording chunk arrives as.
    fn flac_of(samples: &[i16]) -> Vec<u8> {
        wav_to_flac(&encode_wav(samples).unwrap()).unwrap()
    }

    /// Decode a FLAC stream with claxon, checking it is 16 kHz mono 16-bit.
    fn decode(flac: &[u8]) -> Vec<i16> {
        let mut reader = claxon::FlacReader::new(flac).expect("valid FLAC");
        let info = reader.streaminfo();
        assert_eq!(info.sample_rate, 16_000);
        assert_eq!(info.channels, 1);
        assert_eq!(info.bits_per_sample, 16);
        let samples: Vec<i16> = reader
            .samples()
            .map(|s| i16::try_from(s.unwrap()).unwrap())
            .collect();
        // Zero total samples means "unknown" in STREAMINFO; claxon says None.
        assert_eq!(info.samples.unwrap_or(0), samples.len() as u64);
        samples
    }

    /// Runs of 3000 samples: silence, a tone, alternating full-scale
    /// extremes, white noise.
    fn mixed(len: usize) -> Vec<i16> {
        let mut state = 0x2545_f491_u32;
        (0..len)
            .map(|i| match (i / 3000) % 4 {
                0 => 0,
                1 => ((i as f64 * 0.17).sin() * 9000.0) as i16,
                2 => {
                    if i % 2 == 0 {
                        i16::MAX
                    } else {
                        i16::MIN
                    }
                }
                _ => {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as i16
                }
            })
            .collect()
    }

    #[test]
    fn flac_round_trips_exactly() {
        // Lengths around flacenc's 4096-sample block: odd tails, one short
        // block, a single sample.
        for len in [4096 * 3 + 1234, 4096, 4097, 4095, 777, 16, 1] {
            let samples = mixed(len);
            let flac = flac_of(&samples);
            assert!(flac.starts_with(b"fLaC"), "len {len}");
            assert_eq!(decode(&flac), samples, "len {len}");
        }
    }

    #[test]
    fn flac_round_trips_silence_and_extremes() {
        for samples in [
            vec![0i16; 16_000],
            vec![i16::MAX; 5000],
            vec![i16::MIN; 5000],
            (0..10_000)
                .map(|i| if i % 2 == 0 { i16::MAX } else { i16::MIN })
                .collect(),
        ] {
            assert_eq!(decode(&flac_of(&samples)), samples);
        }
    }

    #[test]
    fn empty_audio_encodes_to_an_empty_stream() {
        let flac = flac_of(&[]);
        assert!(flac.starts_with(b"fLaC"));
        assert!(decode(&flac).is_empty());
    }

    #[test]
    fn frame_by_frame_matches_the_one_call_encoder() {
        let samples = mixed(4096 * 5 + 321);
        let config = flacenc::config::Encoder::default().into_verified().unwrap();
        let wide: Vec<i32> = samples.iter().map(|&s| i32::from(s)).collect();
        let source = flacenc::source::MemSource::from_samples(
            &wide,
            CHANNELS,
            BITS_PER_SAMPLE,
            SEGMENT_SAMPLE_RATE,
        );
        let mut stream =
            flacenc::encode_with_fixed_block_size(&config, source, config.block_size).unwrap();
        // The one deliberate difference: the fixed block size as the minimum.
        assert_eq!(stream.stream_info().min_block_size(), 321);
        stream
            .stream_info_mut()
            .set_block_sizes(config.block_size, config.block_size)
            .unwrap();
        let mut sink = ByteSink::new();
        stream.write(&mut sink).unwrap();

        assert_eq!(flac_of(&samples), sink.into_inner());
    }

    /// A WAV of `samples` in `spec`, as hound writes it.
    fn wav_in(spec: hound::WavSpec, samples: &[i16]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
        for &s in samples {
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();
        cursor.into_inner()
    }

    #[test]
    fn other_wav_formats_are_refused() {
        let recording = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let samples = mixed(5000);
        assert_eq!(
            decode(&wav_to_flac(&wav_in(recording, &samples)).unwrap()),
            samples
        );
        for spec in [
            hound::WavSpec {
                sample_rate: 8000,
                ..recording
            },
            hound::WavSpec {
                channels: 2,
                ..recording
            },
            hound::WavSpec {
                bits_per_sample: 8,
                ..recording
            },
        ] {
            let samples: Vec<i16> = (0..4000).map(|i| (i % 100) as i16).collect();
            assert!(wav_to_flac(&wav_in(spec, &samples)).is_err(), "{spec:?}");
        }
    }

    #[test]
    fn the_data_chunk_is_found_after_other_chunks() {
        // RIFF header and fmt chunk, then a LIST chunk, then the data chunk.
        let samples = mixed(9000);
        let plain = encode_wav(&samples).unwrap();
        let list = b"LIST\x0c\x00\x00\x00INFOISFT\x00\x00\x00\x00";
        let mut wav = plain[..36].to_vec();
        wav.extend_from_slice(list);
        wav.extend_from_slice(&plain[36..]);
        let riff_len = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_len.to_le_bytes());

        assert_eq!(wav_to_flac(&wav).unwrap(), flac_of(&samples));
    }

    #[test]
    fn sixteen_bits_in_a_wider_slot_are_refused() {
        // WAVE_FORMAT_EXTENSIBLE: 16 valid bits in 3-byte slots, which hound
        // reports as 16-bit.
        let n = 1000u32;
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&(4 + 8 + 40 + 8 + n * 3).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&40u32.to_le_bytes());
        wav.extend_from_slice(&0xfffe_u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&3u16.to_le_bytes());
        wav.extend_from_slice(&24u16.to_le_bytes());
        wav.extend_from_slice(&22u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(&4u32.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM
        wav.extend_from_slice(&[
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(n * 3).to_le_bytes());
        wav.resize(wav.len() + n as usize * 3, 0);

        let spec = hound::WavReader::new(&wav[..]).unwrap().spec();
        assert_eq!(spec.bits_per_sample, 16);
        let err = wav_to_flac(&wav).unwrap_err();
        assert!(err.to_string().contains("2 bytes wide"), "{err}");
    }

    #[test]
    fn a_truncated_wav_is_refused() {
        let wav = encode_wav(&mixed(9000)).unwrap();
        assert!(wav_to_flac(&wav[..wav.len() - 100]).is_err());
    }

    #[test]
    fn speech_band_audio_compresses() {
        let samples: Vec<i16> = (0..160_000)
            .map(|i| ((i as f64 * 0.05).sin() * 3000.0) as i16)
            .collect();
        let wav = encode_wav(&samples).unwrap();
        let flac = wav_to_flac(&wav).unwrap();
        assert!(
            flac.len() * 2 < wav.len(),
            "{} vs {}",
            flac.len(),
            wav.len()
        );
    }

    #[tokio::test]
    async fn recording_upload_is_flac() {
        let samples = mixed(20_000);
        let wav = encode_wav(&samples).unwrap();
        let upload = recording_upload(&wav).await;
        assert_eq!(upload.format, UploadFormat::FLAC);
        assert_eq!(decode(&upload.bytes), samples);
    }

    #[tokio::test]
    async fn recording_upload_falls_back_to_the_wav() {
        let not_wav = b"RIFF but not really a WAV".to_vec();
        let upload = recording_upload(&not_wav).await;
        assert_eq!(upload.format, UploadFormat::WAV);
        assert_eq!(&*upload.bytes, &not_wav[..]);

        let stereo = wav_in(
            hound::WavSpec {
                channels: 2,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
            &mixed(8000),
        );
        let upload = recording_upload(&stereo).await;
        assert_eq!(upload.format, UploadFormat::WAV);
        assert_eq!(&*upload.bytes, &stereo[..]);

        let upload = recording_upload(&[]).await;
        assert_eq!(upload.format, UploadFormat::WAV);
        assert!(upload.bytes.is_empty());
    }
}
