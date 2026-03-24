/// eve — Encoded VoIP Exfil
///
/// Transfers arbitrary files by encoding them as mFSK audio streamed
/// over a SIP/RTP VoIP call.
mod codec;
mod config;
mod framing;
mod pipeline;
mod transport;
mod voip;
mod wav;

use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(
    name = "eve",
    about = "Encoded VoIP Exfil — mFSK file transfer over SIP/RTP"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Number of mFSK tones (2|4|8|16|32)
    #[arg(long, global = true, default_value_t = 16, value_parser = parse_tones)]
    tones: u8,

    /// Symbols per second
    #[arg(long, global = true, default_value_t = 50)]
    symbol_rate: u32,

    /// Lowest tone frequency in Hz
    #[arg(long, global = true, default_value_t = 400.0)]
    base_freq: f64,

    /// Spacing between tones in Hz
    #[arg(long, global = true, default_value_t = 100.0)]
    tone_spacing: f64,

    /// Payload bytes per frame
    #[arg(long, global = true, default_value_t = 128)]
    frame_size: usize,

    /// Local SIP port
    #[arg(long, global = true, default_value_t = 5060)]
    sip_port: u16,

    /// Local RTP port
    #[arg(long, global = true, default_value_t = 10000)]
    rtp_port: u16,

    /// Dejitter buffer depth in milliseconds
    #[arg(long, global = true, default_value_t = 60)]
    jitter_ms: u32,

    /// Print per-symbol diagnostics
    #[arg(long, global = true)]
    verbose: bool,

    /// Dump generated PCM to a WAV file for debugging
    #[arg(long, global = true, value_name = "PATH")]
    save_audio: Option<PathBuf>,

    /// Start signal tone frequency in Hz (must be below --base-freq)
    #[arg(long, global = true, default_value_t = 300.0)]
    start_tone_freq: f64,

    /// Stop signal tone frequency in Hz (must be above the highest data tone)
    #[arg(long, global = true, default_value_t = 3600.0)]
    stop_tone_freq: f64,

    /// Duration of each signal tone in milliseconds
    #[arg(long, global = true, default_value_t = 200)]
    signal_tone_ms: u32,

    /// ARQ retransmission rounds (0 = disabled)
    #[arg(long, global = true, default_value_t = 3)]
    arq_retries: u32,

    /// Milliseconds sender waits for a NAK before giving up
    #[arg(long, global = true, default_value_t = 2000)]
    arq_timeout: u64,
}

#[derive(Subcommand)]
enum Command {
    /// Send a file to a receiver
    Send {
        /// File to transfer
        #[arg(long, short)]
        file: PathBuf,

        /// Receiver address (IP:SIP-port)
        #[arg(long, short)]
        dest: SocketAddr,
    },
    /// Receive a file from a sender
    Recv {
        /// Directory to write received files into
        #[arg(long, short)]
        output_dir: PathBuf,

        /// Keep listening for additional transfers after each file completes
        #[arg(long)]
        persist: bool,
    },
    /// Run a local loopback test (encode then decode without network)
    Loopback {
        /// File to round-trip through the codec
        #[arg(long, short)]
        file: PathBuf,

        /// Fraction of frame bytes to randomly drop before decoding (0.0–1.0)
        #[arg(long, default_value_t = 0.0)]
        loss_rate: f64,
    },
}

fn parse_tones(s: &str) -> Result<u8, String> {
    match s.parse::<u8>() {
        Ok(n) if matches!(n, 2 | 4 | 8 | 16 | 32) => Ok(n),
        _ => Err(format!(
            "{s} is not a valid tone count; use 2, 4, 8, 16, or 32"
        )),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    let codec_cfg = config::CodecConfig {
        tones: cli.tones,
        symbol_rate: cli.symbol_rate,
        base_freq: cli.base_freq,
        tone_spacing: cli.tone_spacing,
        sample_rate: 8000,
        start_tone_freq: cli.start_tone_freq,
        stop_tone_freq: cli.stop_tone_freq,
        signal_tone_ms: cli.signal_tone_ms,
    };
    let framing_cfg = config::FramingConfig {
        frame_size: cli.frame_size,
    };
    let voip_cfg = config::VoipConfig {
        sip_port: cli.sip_port,
        rtp_port: cli.rtp_port,
        jitter_ms: cli.jitter_ms,
    };
    let arq_cfg = config::ArqConfig {
        retries: cli.arq_retries,
        timeout_ms: cli.arq_timeout,
    };
    match cli.command {
        Command::Send { file, dest } => {
            let cfg = config::Config {
                codec: codec_cfg,
                framing: framing_cfg,
                voip: voip_cfg,
                arq: arq_cfg,
                verbose: cli.verbose,
                save_audio: cli.save_audio,
                persist: false,
            };
            pipeline::run_sender(file, dest, cfg).await;
        }
        Command::Recv { output_dir, persist } => {
            let cfg = config::Config {
                codec: codec_cfg,
                framing: framing_cfg,
                voip: voip_cfg,
                arq: arq_cfg,
                verbose: cli.verbose,
                save_audio: cli.save_audio,
                persist,
            };
            pipeline::run_receiver(output_dir, cfg).await;
        }
        Command::Loopback { file, loss_rate } => {
            let cfg = config::Config {
                codec: codec_cfg,
                framing: framing_cfg,
                voip: voip_cfg,
                arq: arq_cfg,
                verbose: cli.verbose,
                save_audio: cli.save_audio,
                persist: false,
            };
            let ok = pipeline::run_loopback(file, cfg, loss_rate);
            std::process::exit(if ok { 0 } else { 1 });
        }
    }
}
