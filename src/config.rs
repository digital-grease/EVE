/// Shared configuration types used across all modules.
/// mFSK codec configuration.
#[derive(Debug, Clone)]
pub struct CodecConfig {
    /// Number of tones (M). Must be 2, 4, 8, 16, or 32.
    pub tones: u8,
    /// Symbol rate in symbols per second.
    pub symbol_rate: u32,
    /// Base (lowest) tone frequency in Hz.
    pub base_freq: f64,
    /// Spacing between adjacent tones in Hz.
    pub tone_spacing: f64,
    /// Sample rate in Hz (fixed at 8000 for G.711 compatibility).
    pub sample_rate: u32,
    /// Frequency of the start signal tone in Hz (should be below base_freq).
    pub start_tone_freq: f64,
    /// Frequency of the stop signal tone in Hz (should be above the highest data tone).
    pub stop_tone_freq: f64,
    /// Duration of each signal tone in milliseconds.
    pub signal_tone_ms: u32,
}

impl Default for CodecConfig {
    fn default() -> Self {
        Self {
            tones: 16,
            symbol_rate: 50,
            base_freq: 400.0,
            tone_spacing: 100.0,
            sample_rate: 8000,
            start_tone_freq: 300.0,
            stop_tone_freq: 3600.0,
            signal_tone_ms: 200,
        }
    }
}

impl CodecConfig {
    /// Number of PCM samples for one signal tone burst.
    pub fn signal_tone_samples(&self) -> usize {
        (self.sample_rate as f64 * self.signal_tone_ms as f64 / 1000.0) as usize
    }
}

impl CodecConfig {
    /// Number of bits encoded per symbol: log2(M).
    #[allow(dead_code)] // Public utility used by external consumers and diagnostics
    pub fn bits_per_symbol(&self) -> u32 {
        (self.tones as f64).log2() as u32
    }

    /// Number of PCM samples per symbol window.
    pub fn samples_per_symbol(&self) -> usize {
        (self.sample_rate / self.symbol_rate) as usize
    }

    /// Frequency for tone index `i`.
    pub fn tone_freq(&self, i: usize) -> f64 {
        self.base_freq + i as f64 * self.tone_spacing
    }
}

/// Framing layer configuration.
#[derive(Debug, Clone)]
pub struct FramingConfig {
    /// Maximum payload bytes per frame.
    pub frame_size: usize,
}

impl Default for FramingConfig {
    fn default() -> Self {
        Self { frame_size: 128 }
    }
}

/// VoIP / network configuration.
#[derive(Debug, Clone)]
pub struct VoipConfig {
    /// Local SIP port.
    pub sip_port: u16,
    /// Local RTP port.
    pub rtp_port: u16,
    /// Dejitter buffer depth in milliseconds.
    pub jitter_ms: u32,
}

impl Default for VoipConfig {
    fn default() -> Self {
        Self {
            sip_port: 5060,
            rtp_port: 10000,
            jitter_ms: 60,
        }
    }
}

/// ARQ (Automatic Repeat reQuest) configuration.
#[derive(Debug, Clone)]
pub struct ArqConfig {
    /// Maximum number of retransmission rounds (0 = ARQ disabled).
    pub retries: u32,
    /// How long the sender waits for a NAK from the receiver, in milliseconds.
    pub timeout_ms: u64,
}

impl Default for ArqConfig {
    fn default() -> Self {
        Self {
            retries: 3,
            timeout_ms: 2000,
        }
    }
}

/// Combined runtime configuration passed through the pipeline.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub codec: CodecConfig,
    pub framing: FramingConfig,
    pub voip: VoipConfig,
    pub arq: ArqConfig,
    /// Print per-symbol diagnostics.
    pub verbose: bool,
    /// Optionally dump raw PCM to a WAV file.
    pub save_audio: Option<std::path::PathBuf>,
    /// Keep the receiver listening after each completed transfer.
    pub persist: bool,
}
