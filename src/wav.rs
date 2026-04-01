/// Minimal WAV file writer (no external crates).
///
/// Writes a standard 44-byte PCM WAV header followed by 16-bit signed
/// little-endian samples at the given sample rate.
use std::io::{self, Write};

/// Write a 16-bit PCM WAV file to `writer`.
///
/// - `sample_rate`: samples per second (e.g. 8000).
/// - `samples`: 16-bit signed PCM samples (mono).
pub fn write_wav<W: Write>(writer: &mut W, sample_rate: u32, samples: &[i16]) -> io::Result<()> {
    let num_channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * num_channels as u32 * bits_per_sample as u32 / 8;
    let block_align: u16 = num_channels * bits_per_sample / 8;
    let data_bytes = samples.len().checked_mul(2).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "WAV data too large")
    })?;
    if data_bytes > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("WAV data size {} exceeds 4 GiB limit", data_bytes),
        ));
    }
    let data_len = data_bytes as u32;
    let chunk_size = 36u32.checked_add(data_len).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "WAV chunk size overflow")
    })?;

    // RIFF header.
    writer.write_all(b"RIFF")?;
    writer.write_all(&chunk_size.to_le_bytes())?;
    writer.write_all(b"WAVE")?;

    // fmt sub-chunk.
    writer.write_all(b"fmt ")?;
    writer.write_all(&16u32.to_le_bytes())?; // sub-chunk size
    writer.write_all(&1u16.to_le_bytes())?; // PCM format
    writer.write_all(&num_channels.to_le_bytes())?;
    writer.write_all(&sample_rate.to_le_bytes())?;
    writer.write_all(&byte_rate.to_le_bytes())?;
    writer.write_all(&block_align.to_le_bytes())?;
    writer.write_all(&bits_per_sample.to_le_bytes())?;

    // data sub-chunk.
    writer.write_all(b"data")?;
    writer.write_all(&data_len.to_le_bytes())?;
    for &s in samples {
        writer.write_all(&s.to_le_bytes())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal WAV parser to verify the header we wrote.
    fn parse_wav_header(data: &[u8]) -> (u32, u16, u32) {
        // Returns (sample_rate, bits_per_sample, data_len).
        assert_eq!(&data[0..4], b"RIFF");
        assert_eq!(&data[8..12], b"WAVE");
        assert_eq!(&data[12..16], b"fmt ");
        let sample_rate = u32::from_le_bytes(data[24..28].try_into().unwrap());
        let bits_per_sample = u16::from_le_bytes(data[34..36].try_into().unwrap());
        assert_eq!(&data[36..40], b"data");
        let data_len = u32::from_le_bytes(data[40..44].try_into().unwrap());
        (sample_rate, bits_per_sample, data_len)
    }

    #[test]
    fn test_write_wav_header() {
        let samples: Vec<i16> = vec![0i16; 8000]; // 1 second of silence.
        let mut buf = Vec::new();
        write_wav(&mut buf, 8000, &samples).unwrap();
        assert_eq!(buf.len(), 44 + 8000 * 2);
        let (sr, bps, dl) = parse_wav_header(&buf);
        assert_eq!(sr, 8000);
        assert_eq!(bps, 16);
        assert_eq!(dl, 8000 * 2);
    }

    #[test]
    fn test_write_wav_empty() {
        let mut buf = Vec::new();
        write_wav(&mut buf, 8000, &[]).unwrap();
        assert_eq!(buf.len(), 44);
        let (_, _, dl) = parse_wav_header(&buf);
        assert_eq!(dl, 0);
    }
}
