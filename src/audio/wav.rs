//! WAV encoding for raw PCM samples.

use anyhow::Context;

use super::capture::{CHANNELS, SAMPLE_RATE};

/// Encode raw PCM samples (16kHz, mono, i16) to a WAV byte buffer.
pub fn encode_wav(samples: &[i16]) -> anyhow::Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer =
            hound::WavWriter::new(&mut cursor, spec).context("failed to create WAV writer")?;

        for &sample in samples {
            writer
                .write_sample(sample)
                .context("failed to write WAV sample")?;
        }

        writer.finalize().context("failed to finalize WAV")?;
    }

    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_wav_produces_valid_output() {
        let samples: Vec<i16> = (0..1600).map(|i| (i % 256) as i16).collect();
        let wav = encode_wav(&samples).unwrap();

        // WAV files start with "RIFF".
        assert_eq!(&wav[..4], b"RIFF");

        // Verify we can read it back with hound.
        let cursor = std::io::Cursor::new(&wav);
        let reader = hound::WavReader::new(cursor).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);

        let read_samples: Vec<i16> = reader.into_samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(read_samples.len(), 1600);
        assert_eq!(read_samples, samples);
    }

    #[test]
    fn encode_wav_empty_samples() {
        let wav = encode_wav(&[]).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
    }
}
