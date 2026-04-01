/// mFSK encoder: converts byte slices to PCM audio samples.
///
/// Supports M = 2, 4, 8, 16, 32 tones.  Each symbol encodes log2(M) bits.
/// Samples are 16-bit signed PCM at 8 000 Hz (G.711 compatible).
///
/// Stream layout:
/// ```text
/// [start tone] [sync preamble] [data symbols] [stop tone]
/// ```
///
/// The start and stop tones are fixed-frequency sinusoids at configurable
/// frequencies outside the mFSK data band (default 300 Hz and 3600 Hz).
/// They let the decoder confirm it is receiving an eve stream before
/// attempting preamble correlation, and mark the precise end of data.
use crate::config::CodecConfig;

/// Number of raised-cosine ramp samples at each symbol boundary.
/// 2 ms × 8 000 Hz = 16 samples.
const RAMP_SAMPLES: usize = 16;

/// mFSK modulator.
pub struct MfskEncoder {
    pub(super) config: CodecConfig,
}

impl MfskEncoder {
    /// Create a new encoder with the given configuration.
    ///
    /// Returns `Err` if `config.tones` is not one of {2, 4, 8, 16, 32}.
    pub fn new(config: CodecConfig) -> Result<Self, super::CodecError> {
        match config.tones {
            2 | 4 | 8 | 16 | 32 => Ok(Self { config }),
            n => Err(super::CodecError::UnsupportedToneCount(n)),
        }
    }

    /// Encode a byte slice to a PCM sample vector.
    ///
    /// Output layout: `[start tone][preamble][data symbols][stop tone]`.
    pub fn encode(&self, data: &[u8]) -> Vec<i16> {
        let bits_per_symbol = self.config.bits_per_symbol() as usize;
        let sps = self.config.samples_per_symbol();

        let mut out: Vec<i16> = Vec::new();

        // 1. Start tone.
        out.extend_from_slice(&self.start_tone_samples());

        // 2. Synchronisation preamble.
        out.extend_from_slice(&self.generate_preamble());

        // 3. Data symbols.
        let bits = bytes_to_bits(data);
        let symbols = bits_to_symbols(&bits, bits_per_symbol);

        let mut phase: f64 = 0.0; // continuous phase across symbols
        for (idx, &sym) in symbols.iter().enumerate() {
            let freq = self.config.tone_freq(sym);
            let next_freq = if idx + 1 < symbols.len() {
                self.config.tone_freq(symbols[idx + 1])
            } else {
                freq
            };
            let segment =
                generate_symbol(freq, next_freq, sps, self.config.sample_rate, &mut phase);
            out.extend_from_slice(&segment);
        }

        // 4. Stop tone.
        out.extend_from_slice(&self.stop_tone_samples());

        out
    }

    /// Generate the start signal tone: a fixed-frequency sinusoid at
    /// `config.start_tone_freq` for `config.signal_tone_ms` milliseconds.
    pub fn start_tone_samples(&self) -> Vec<i16> {
        signal_tone(
            self.config.start_tone_freq,
            self.config.signal_tone_samples(),
            self.config.sample_rate,
        )
    }

    /// Generate the stop signal tone: a fixed-frequency sinusoid at
    /// `config.stop_tone_freq` for `config.signal_tone_ms` milliseconds.
    pub fn stop_tone_samples(&self) -> Vec<i16> {
        signal_tone(
            self.config.stop_tone_freq,
            self.config.signal_tone_samples(),
            self.config.sample_rate,
        )
    }

    /// Generate the synchronisation preamble (chirp sweep, used by tests).
    pub fn preamble_samples(&self) -> Vec<i16> {
        self.generate_preamble()
    }

    fn generate_preamble(&self) -> Vec<i16> {
        let m = self.config.tones as usize;
        let sps = self.config.samples_per_symbol();
        let mut out = Vec::with_capacity(2 * m * sps);
        let mut phase: f64 = 0.0;

        // Ascending sweep: tone 0 … M-1.
        for i in 0..m {
            let freq = self.config.tone_freq(i);
            let next_freq = self.config.tone_freq(if i + 1 < m { i + 1 } else { i });
            let seg = generate_symbol(freq, next_freq, sps, self.config.sample_rate, &mut phase);
            out.extend_from_slice(&seg);
        }
        // Descending sweep: tone M-1 … 0.
        for i in (0..m).rev() {
            let freq = self.config.tone_freq(i);
            let next_freq = self.config.tone_freq(if i > 0 { i - 1 } else { 0 });
            let seg = generate_symbol(freq, next_freq, sps, self.config.sample_rate, &mut phase);
            out.extend_from_slice(&seg);
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Generate a pure sinusoid at `freq` Hz for `n_samples` samples at `sample_rate`.
pub(super) fn signal_tone(freq: f64, n_samples: usize, sample_rate: u32) -> Vec<i16> {
    let sr = sample_rate as f64;
    let ramp = (n_samples / 8).max(1); // 12.5% attack/decay ramp
    (0..n_samples)
        .map(|n| {
            let amp = if n < ramp {
                0.5 * (1.0 - (std::f64::consts::PI * n as f64 / ramp as f64).cos())
            } else if n >= n_samples - ramp {
                let t = (n - (n_samples - ramp)) as f64 / ramp as f64;
                0.5 * (1.0 + (std::f64::consts::PI * t).cos())
            } else {
                1.0
            };
            let phase = 2.0 * std::f64::consts::PI * freq * n as f64 / sr;
            (amp * phase.sin() * i16::MAX as f64) as i16
        })
        .collect()
}

/// Convert a byte slice to a flat bit vector (MSB first).
fn bytes_to_bits(data: &[u8]) -> Vec<u8> {
    let mut bits = Vec::with_capacity(data.len() * 8);
    for &byte in data {
        for shift in (0..8).rev() {
            bits.push((byte >> shift) & 1);
        }
    }
    bits
}

/// Pack a flat bit vector into symbol indices of width `bits_per_symbol`.
///
/// Trailing bits are zero-padded to reach a full symbol boundary.
fn bits_to_symbols(bits: &[u8], bits_per_symbol: usize) -> Vec<usize> {
    let n_symbols = bits.len().div_ceil(bits_per_symbol);
    let mut symbols = Vec::with_capacity(n_symbols);
    for chunk in bits.chunks(bits_per_symbol) {
        let mut val: usize = 0;
        for &b in chunk {
            val = (val << 1) | b as usize;
        }
        // Zero-pad if the last chunk is short.
        val <<= bits_per_symbol - chunk.len();
        symbols.push(val);
    }
    symbols
}

/// Generate PCM samples for one symbol at `freq` Hz with a raised-cosine ramp
/// blending into the `next_freq` tone at the end of the window.
fn generate_symbol(
    freq: f64,
    next_freq: f64,
    sps: usize,
    sample_rate: u32,
    phase: &mut f64,
) -> Vec<i16> {
    let sr = sample_rate as f64;
    let ramp = RAMP_SAMPLES.min(sps / 4);
    let mut samples = Vec::with_capacity(sps);

    let ramp_div = (ramp as f64 - 1.0).max(1.0); // ensures ramp reaches exactly 0/1 at boundaries

    for n in 0..sps {
        let amp = if n < ramp {
            0.5 * (1.0 - (std::f64::consts::PI * n as f64 / ramp_div).cos())
        } else if n >= sps - ramp {
            let t = (n - (sps - ramp)) as f64 / ramp_div;
            0.5 * (1.0 + (std::f64::consts::PI * t).cos())
        } else {
            1.0
        };

        let inst_freq = if n >= sps - ramp && ramp > 0 {
            let t = (n - (sps - ramp)) as f64 / ramp_div;
            freq + t * (next_freq - freq)
        } else {
            freq
        };

        *phase += 2.0 * std::f64::consts::PI * inst_freq / sr;
        *phase = phase.rem_euclid(2.0 * std::f64::consts::PI);

        let sample = amp * phase.sin() * i16::MAX as f64;
        samples.push(sample as i16);
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CodecConfig;

    fn default_config(tones: u8) -> CodecConfig {
        CodecConfig {
            tones,
            ..CodecConfig::default()
        }
    }

    #[test]
    fn test_unsupported_tones() {
        assert!(MfskEncoder::new(default_config(3)).is_err());
        assert!(MfskEncoder::new(default_config(7)).is_err());
    }

    #[test]
    fn test_supported_tones() {
        for &m in &[2u8, 4, 8, 16, 32] {
            assert!(MfskEncoder::new(default_config(m)).is_ok());
        }
    }

    #[test]
    fn test_encode_includes_signal_tones() {
        let cfg = CodecConfig::default(); // M=16, signal_tone_ms=200
        let enc = MfskEncoder::new(cfg.clone()).unwrap();
        let samples = enc.encode(b"Hello");

        let start_len = cfg.signal_tone_samples();
        let preamble_len = 2 * cfg.tones as usize * cfg.samples_per_symbol();
        let stop_len = cfg.signal_tone_samples();

        // Output must be at least start + preamble + stop.
        assert!(samples.len() >= start_len + preamble_len + stop_len);
    }

    #[test]
    fn test_encode_empty_has_tones_and_preamble() {
        let cfg = CodecConfig::default();
        let enc = MfskEncoder::new(cfg.clone()).unwrap();
        let samples = enc.encode(b"");

        let start_len = cfg.signal_tone_samples();
        let preamble_len = 2 * cfg.tones as usize * cfg.samples_per_symbol();
        let stop_len = cfg.signal_tone_samples();

        assert_eq!(samples.len(), start_len + preamble_len + stop_len);
    }

    #[test]
    fn test_start_tone_length() {
        let cfg = CodecConfig::default(); // 200ms at 8kHz = 1600 samples
        let enc = MfskEncoder::new(cfg.clone()).unwrap();
        assert_eq!(enc.start_tone_samples().len(), cfg.signal_tone_samples());
        assert_eq!(enc.stop_tone_samples().len(), cfg.signal_tone_samples());
    }

    #[test]
    fn test_bits_to_symbols_padding() {
        let bits = vec![1u8, 0, 1];
        let syms = bits_to_symbols(&bits, 4);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0], 10);
    }

    #[test]
    fn test_bytes_to_bits_roundtrip() {
        let data = b"AB";
        let bits = bytes_to_bits(data);
        assert_eq!(bits.len(), 16);
        assert_eq!(&bits[..8], &[0, 1, 0, 0, 0, 0, 0, 1]);
    }
}
