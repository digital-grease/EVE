/// Raw UDP socket abstraction for SIP and RTP traffic.
pub mod udp;


/// Errors produced by the transport layer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bind failed on port {port}: {source}")]
    BindFailed { port: u16, source: std::io::Error },
}
