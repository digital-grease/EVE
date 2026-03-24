/// mFSK codec: encode bytes to PCM samples and decode PCM samples back to bytes.
pub mod mfsk_decode;
pub mod mfsk_encode;

pub use mfsk_decode::MfskDecoder;
pub use mfsk_encode::MfskEncoder;

/// Errors produced by the codec layer.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("unsupported tone count {0}; must be 2, 4, 8, 16, or 32")]
    UnsupportedToneCount(u8),
    #[error("start tone not found — not an eve stream")]
    StartToneNotFound,
    #[error("preamble not found in audio stream")]
    PreambleNotFound,
    #[error("input too short to decode")]
    InputTooShort,
}

#[cfg(test)]
mod roundtrip_tests {
    use super::{MfskDecoder, MfskEncoder};
    use crate::config::CodecConfig;

    fn config(tones: u8) -> CodecConfig {
        CodecConfig {
            tones,
            ..CodecConfig::default()
        }
    }

    fn roundtrip(tones: u8, data: &[u8]) {
        let enc = MfskEncoder::new(config(tones)).unwrap();
        let dec = MfskDecoder::new(config(tones)).unwrap();
        let samples = enc.encode(data);
        let decoded = dec.decode(&samples).expect("decode failed");
        // The decoded output may be slightly longer due to symbol-boundary padding;
        // the first `data.len()` bytes must match exactly.
        assert!(
            decoded.len() >= data.len(),
            "decoded {} bytes, expected at least {}",
            decoded.len(),
            data.len()
        );
        assert_eq!(
            &decoded[..data.len()],
            data,
            "round-trip mismatch at M={tones}"
        );
    }

    #[test]
    fn roundtrip_m2() {
        roundtrip(2, b"AB");
    }

    #[test]
    fn roundtrip_m4() {
        roundtrip(4, b"Hello");
    }

    #[test]
    fn roundtrip_m8() {
        roundtrip(8, b"mFSK test data 8");
    }

    #[test]
    fn roundtrip_m16() {
        roundtrip(16, b"The quick brown fox");
    }

    #[test]
    fn roundtrip_m32() {
        roundtrip(32, b"eve encodes data over VoIP");
    }

    #[test]
    fn roundtrip_all_byte_values() {
        // Encode one of each byte value 0x00..=0xFF with M=16.
        let data: Vec<u8> = (0u8..=255).collect();
        roundtrip(16, &data);
    }
}
