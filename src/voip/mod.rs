/// VoIP transport: SIP signalling and RTP media.
pub mod rtp;
pub mod sip;


/// Errors produced by the VoIP layer.
#[derive(Debug, thiserror::Error)]
pub enum VoipError {
    #[error("SIP error: {0}")]
    Sip(String),
    #[error("RTP error: {0}")]
    Rtp(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
