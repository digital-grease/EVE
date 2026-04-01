/// mFSK decoder: converts PCM audio samples back to bytes.
///
/// ## Algorithm
/// 1. Confirm start signal tone in the leading window.
/// 2. Detect the sync preamble with a cross-correlation matched filter.
/// 3. Segment remaining audio into symbol-length windows.
/// 4. For each window run a Goertzel filter at each of the M tone frequencies.
/// 5. Pick the bin with highest energy, map back to bits, reassemble bytes.
/// 6. Stop demodulating when a stop signal tone window is detected.
use crate::config::CodecConfig;

/// Minimum fraction of window energy that must come from the target frequency
/// for `detect_signal_tone` to return `true`.  0.5 = 50% of total energy.
const TONE_DETECT_FRACTION: f64 = 0.5;

/// The stop-tone Goertzel energy must exceed the best data-tone Goertzel
/// energy by this factor to count as a stop tone.  A value of 2.0 means the
/// stop-tone bin must carry at least twice the energy of the strongest data
/// bin.  This is robust to μ-law harmonic distortion, which can raise the
/// stop-tone bin to a moderate fraction of window energy even during data
/// symbols, but cannot make it exceed the dominant data-tone bin.
const STOP_TONE_DATA_RATIO: f64 = 2.0;

/// mFSK demodulator.
pub struct MfskDecoder {
    config: CodecConfig,
}

impl MfskDecoder {
    /// Create a new decoder with the given configuration.
    pub fn new(config: CodecConfig) -> Result<Self, super::CodecError> {
        match config.tones {
            2 | 4 | 8 | 16 | 32 => Ok(Self { config }),
            n => Err(super::CodecError::UnsupportedToneCount(n)),
        }
    }

    /// Decode PCM samples to bytes.
    ///
    /// Expects the layout produced by the encoder:
    /// `[start tone][preamble][data symbols][stop tone]`.
    ///
    /// Returns `Err(StartToneNotFound)` if the leading window does not contain
    /// the start signal tone — this lets a persist-mode receiver skip non-eve
    /// audio without attempting preamble correlation.
    pub fn decode(&self, samples: &[i16]) -> Result<Vec<u8>, super::CodecError> {
        let (bytes, _) = self.decode_inner(samples, false)?;
        Ok(bytes)
    }

    /// Decode PCM samples to bytes with per-symbol SNR diagnostics.
    ///
    /// Returns `(decoded_bytes, snr_ratios)` where each SNR ratio is
    /// `best_energy / second_best_energy` for the corresponding symbol.
    /// Higher values indicate more confident tone detection.
    pub fn decode_verbose(
        &self,
        samples: &[i16],
    ) -> Result<(Vec<u8>, Vec<f64>), super::CodecError> {
        self.decode_inner(samples, true)
    }

    fn decode_inner(
        &self,
        samples: &[i16],
        collect_snr: bool,
    ) -> Result<(Vec<u8>, Vec<f64>), super::CodecError> {
        let bits_per_symbol = self.config.bits_per_symbol() as usize;
        let sps = self.config.samples_per_symbol();
        let tone_window = self.config.signal_tone_samples();

        // 1. Confirm start signal tone in the leading window.
        if samples.len() < tone_window {
            return Err(super::CodecError::InputTooShort);
        }
        if !detect_signal_tone(
            &samples[..tone_window],
            self.config.start_tone_freq,
            self.config.sample_rate,
            TONE_DETECT_FRACTION,
        ) {
            return Err(super::CodecError::StartToneNotFound);
        }

        // 2. Build the reference preamble and locate its end via cross-correlation.
        let reference = self.build_reference_preamble();
        if samples.len() < reference.len() {
            return Err(super::CodecError::InputTooShort);
        }
        let data_start = self.find_preamble(samples, &reference)?;

        // 3. Demodulate symbol by symbol, stopping at the stop signal tone.
        let data_samples = &samples[data_start..];
        if data_samples.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let n_symbols = data_samples.len() / sps;
        let mut symbol_bits: Vec<u8> = Vec::with_capacity(n_symbols * bits_per_symbol);
        let mut snr_ratios: Vec<f64> = if collect_snr {
            Vec::with_capacity(n_symbols)
        } else {
            Vec::new()
        };

        for i in 0..n_symbols {
            let window = &data_samples[i * sps..(i + 1) * sps];

            // Stop demodulating if this window carries the stop signal tone.
            if self.is_stop_tone_window(window) {
                break;
            }

            let tone_idx = self.detect_tone(window);

            if collect_snr {
                let (best, second) = self.snr_estimate(window);
                let ratio = if second > 0.0 { best / second } else { f64::INFINITY };
                snr_ratios.push(ratio);
            }

            // Convert tone index to bits (MSB first).
            for shift in (0..bits_per_symbol).rev() {
                symbol_bits.push(((tone_idx >> shift) & 1) as u8);
            }
        }

        // Pack bits into bytes (ignore trailing padding bits).
        let n_bytes = symbol_bits.len() / 8;
        let bytes = bits_to_bytes(&symbol_bits[..n_bytes * 8]);
        Ok((bytes, snr_ratios))
    }

    /// Detect which of the M tones is most energetic in `window` using a
    /// Goertzel filter (an efficient single-bin DFT).
    ///
    /// Returns the tone index (0 … M-1).
    pub fn detect_tone(&self, window: &[i16]) -> usize {
        let m = self.config.tones as usize;
        let mut best_idx = 0;
        let mut best_energy = f64::NEG_INFINITY;

        for i in 0..m {
            let energy = goertzel(
                window,
                self.config.tone_freq(i),
                self.config.sample_rate as f64,
            );
            if energy > best_energy {
                best_energy = energy;
                best_idx = i;
            }
        }
        best_idx
    }

    /// Compute the SNR estimate for a tone detection result in `window`.
    ///
    /// Returns `(best_energy, second_best_energy)` so the caller can compute a
    /// confidence ratio.
    pub fn snr_estimate(&self, window: &[i16]) -> (f64, f64) {
        let m = self.config.tones as usize;
        let mut energies: Vec<f64> = (0..m)
            .map(|i| {
                goertzel(
                    window,
                    self.config.tone_freq(i),
                    self.config.sample_rate as f64,
                )
            })
            .collect();
        energies.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        (
            energies[0],
            if energies.len() > 1 { energies[1] } else { 0.0 },
        )
    }

    /// Returns `true` if `window` looks like the stop signal tone rather than
    /// a data symbol.
    ///
    /// Compares the Goertzel energy at `stop_tone_freq` against the maximum
    /// Goertzel energy across all M data-tone frequencies.  A genuine stop
    /// tone has very high energy in the stop bin and near-zero energy in every
    /// data bin; a data symbol has one dominant data bin that far exceeds the
    /// stop-tone bin even when μ-law harmonic distortion leaks some energy
    /// into the stop-tone bin.
    fn is_stop_tone_window(&self, window: &[i16]) -> bool {
        let m = self.config.tones as usize;
        let sr = self.config.sample_rate as f64;
        let stop_e = goertzel(window, self.config.stop_tone_freq, sr);
        let max_data_e = (0..m)
            .map(|i| goertzel(window, self.config.tone_freq(i), sr))
            .fold(0.0_f64, f64::max);
        // Guard against silence / very low energy windows.
        max_data_e > 0.0 && stop_e > max_data_e * STOP_TONE_DATA_RATIO
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Regenerate the reference preamble signal for cross-correlation.
    fn build_reference_preamble(&self) -> Vec<i16> {
        // Use the encoder's preamble_samples method by temporarily constructing an
        // encoder; this guarantees the reference exactly matches the transmitted preamble.
        use crate::codec::mfsk_encode::MfskEncoder;
        // Safety: tones are already validated in MfskDecoder::new.
        let enc = MfskEncoder::new(self.config.clone()).expect("validated tones");
        enc.preamble_samples()
    }

    /// Find the sample offset where data begins (i.e. end of preamble) using
    /// normalised cross-correlation.
    ///
    /// Returns the offset into `samples` at which data symbols start.
    fn find_preamble(
        &self,
        samples: &[i16],
        reference: &[i16],
    ) -> Result<usize, super::CodecError> {
        let ref_len = reference.len();
        let search_len = samples.len().saturating_sub(ref_len) + 1;

        if search_len == 0 {
            return Err(super::CodecError::PreambleNotFound);
        }

        // Start searching after the start tone to avoid false correlation peaks.
        let tone_window = self.config.signal_tone_samples();
        let search_start = tone_window.min(search_len.saturating_sub(1));

        // Cap the search window to prevent O(N²) stall on large inputs.
        // The preamble should be within 2x the expected position.
        let expected_preamble_len = 2 * self.config.tones as usize * self.config.samples_per_symbol();
        let search_end = search_len.min(search_start + 3 * expected_preamble_len);

        // Precompute reference energy for normalisation.
        let ref_energy: f64 = reference.iter().map(|&s| (s as f64).powi(2)).sum();
        if ref_energy == 0.0 {
            return Err(super::CodecError::PreambleNotFound);
        }

        let mut best_score = f64::NEG_INFINITY;
        let mut best_offset = 0usize;

        for offset in search_start..search_end {
            let window = &samples[offset..offset + ref_len];
            let cross: f64 = window
                .iter()
                .zip(reference.iter())
                .map(|(&s, &r)| s as f64 * r as f64)
                .sum();
            let win_energy: f64 = window.iter().map(|&s| (s as f64).powi(2)).sum();
            // Normalised cross-correlation coefficient.
            let score = if win_energy > 0.0 {
                cross / (win_energy.sqrt() * ref_energy.sqrt())
            } else {
                0.0
            };

            if score > best_score {
                best_score = score;
                best_offset = offset;
            }
        }

        // Require a minimum correlation score to accept the preamble.
        // A perfect match yields 1.0; allow down to 0.5 for noisy channels.
        if best_score < 0.5 {
            return Err(super::CodecError::PreambleNotFound);
        }

        Ok(best_offset + ref_len)
    }
}

// ---------------------------------------------------------------------------
// Signal processing helpers
// ---------------------------------------------------------------------------

/// Returns `true` if the Goertzel energy at `freq` Hz accounts for at least
/// `fraction` of the total window energy.
///
/// A `fraction` of 0.3 (30 %) reliably detects clean tones while rejecting
/// noise and mFSK data symbols, whose energy is spread across many frequencies.
pub(super) fn detect_signal_tone(
    window: &[i16],
    freq: f64,
    sample_rate: u32,
    fraction: f64,
) -> bool {
    let window_energy: f64 = window.iter().map(|&s| (s as f64).powi(2)).sum();
    if window_energy == 0.0 {
        return false;
    }
    let tone_energy = goertzel(window, freq, sample_rate as f64);
    // Normalize Goertzel output to the same scale as window energy.
    // Goertzel returns |X[k]|^2 which scales as (N/2)^2 * A^2 for a pure
    // sine of amplitude A.  Window energy scales as N/2 * A^2.  Dividing
    // by N/2 makes both comparable.
    let n = window.len() as f64;
    let normalised = tone_energy / (n / 2.0).max(1.0);
    normalised / window_energy > fraction
}

/// Goertzel filter: efficient single-bin DFT energy estimate.
///
/// Returns the squared magnitude of the DFT at `freq` Hz over `samples`.
fn goertzel(samples: &[i16], freq: f64, sample_rate: f64) -> f64 {
    let n = samples.len() as f64;
    let k = (freq / sample_rate * n).round();
    let omega = 2.0 * std::f64::consts::PI * k / n;
    let coeff = 2.0 * omega.cos();

    let mut s_prev = 0.0f64;
    let mut s_prev2 = 0.0f64;

    for &x in samples {
        let s = x as f64 + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }

    // Power = s_prev^2 + s_prev2^2 - coeff * s_prev * s_prev2.
    s_prev.powi(2) + s_prev2.powi(2) - coeff * s_prev * s_prev2
}

/// Pack a flat bit vector (MSB first) into bytes.
///
/// `bits.len()` must be a multiple of 8.
fn bits_to_bytes(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|chunk| chunk.iter().fold(0u8, |acc, &b| (acc << 1) | b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::mfsk_encode::MfskEncoder;
    use crate::config::CodecConfig;

    fn cfg(tones: u8) -> CodecConfig {
        CodecConfig {
            tones,
            ..CodecConfig::default()
        }
    }

    #[test]
    fn test_unsupported_tones() {
        assert!(MfskDecoder::new(cfg(3)).is_err());
    }

    #[test]
    fn test_goertzel_picks_correct_tone() {
        // Generate a pure sine at 800 Hz for 160 samples (M=16, tone index 4).
        let freq = 800.0f64;
        let sr = 8000.0f64;
        let sps = 160usize;
        let samples: Vec<i16> = (0..sps)
            .map(|n| {
                let s = (2.0 * std::f64::consts::PI * freq * n as f64 / sr).sin() * 30_000.0;
                s as i16
            })
            .collect();

        let dec = MfskDecoder::new(cfg(16)).unwrap();
        let idx = dec.detect_tone(&samples);
        // tone_freq(4) = 400 + 4*100 = 800 Hz.
        assert_eq!(idx, 4);
    }

    #[test]
    fn test_bits_to_bytes() {
        // 0100_0001 = 'A' = 65.
        let bits = vec![0u8, 1, 0, 0, 0, 0, 0, 1];
        let bytes = bits_to_bytes(&bits);
        assert_eq!(bytes, vec![0x41]);
    }

    #[test]
    fn test_detect_signal_tone_present() {
        // Pure 300 Hz sine at full amplitude for 1600 samples (200 ms).
        let freq = 300.0f64;
        let sr = 8000u32;
        let n = 1600usize;
        let samples: Vec<i16> = (0..n)
            .map(|i| {
                let s = (2.0 * std::f64::consts::PI * freq * i as f64 / sr as f64).sin()
                    * i16::MAX as f64;
                s as i16
            })
            .collect();
        assert!(detect_signal_tone(&samples, freq, sr, TONE_DETECT_FRACTION));
    }

    #[test]
    fn test_detect_signal_tone_absent() {
        // Pure 900 Hz sine — should NOT be detected as 300 Hz.
        let sr = 8000u32;
        let n = 1600usize;
        let samples: Vec<i16> = (0..n)
            .map(|i| {
                let s = (2.0 * std::f64::consts::PI * 900.0 * i as f64 / sr as f64).sin()
                    * i16::MAX as f64;
                s as i16
            })
            .collect();
        assert!(!detect_signal_tone(&samples, 300.0, sr, TONE_DETECT_FRACTION));
    }

    #[test]
    fn test_start_tone_not_found_on_silence() {
        let config = cfg(16);
        let dec = MfskDecoder::new(config.clone()).unwrap();
        // Feed enough silence that we pass the length check but have no tone.
        let enc = MfskEncoder::new(config).unwrap();
        let samples = enc.encode(b"x");
        // Silence of the same length → no start tone.
        let silence: Vec<i16> = vec![0i16; samples.len()];
        let err = dec.decode(&silence).unwrap_err();
        assert!(
            matches!(err, super::super::CodecError::StartToneNotFound),
            "expected StartToneNotFound, got {err:?}"
        );
    }

    #[test]
    fn test_start_tone_present_after_encode() {
        let config = cfg(16);
        let enc = MfskEncoder::new(config.clone()).unwrap();
        let start_samples = enc.start_tone_samples();
        // The start tone window should be detected at the configured frequency.
        assert!(detect_signal_tone(
            &start_samples,
            config.start_tone_freq,
            config.sample_rate,
            TONE_DETECT_FRACTION,
        ));
    }

    #[test]
    fn test_stop_tone_present_after_encode() {
        let config = cfg(16);
        let enc = MfskEncoder::new(config.clone()).unwrap();
        let stop_samples = enc.stop_tone_samples();
        // The stop tone window (away from ramps) should be detected.
        let mid = stop_samples.len() / 4;
        let sps = config.samples_per_symbol();
        assert!(detect_signal_tone(
            &stop_samples[mid..mid + sps],
            config.stop_tone_freq,
            config.sample_rate,
            TONE_DETECT_FRACTION,
        ));
    }
}
