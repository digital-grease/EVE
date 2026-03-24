/// Framing layer: packetize files into frames and reassemble them.
pub mod depacketizer;
pub mod packetizer;


/// Magic bytes identifying an eve frame.
pub const MAGIC: u16 = 0xEF01;

/// Bit flags in the frame flags byte.
pub mod flags {
    /// First frame; payload is SYN metadata (filename + total size as JSON).
    pub const SYN: u8 = 0b0000_0001;
    /// Last frame.
    pub const FIN: u8 = 0b0000_0010;
    /// NAK: payload is a list of big-endian u32 sequence numbers the receiver is missing.
    pub const NAK: u8 = 0b0000_0100;
}

/// A single framing unit transmitted over the audio channel.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Incrementing sequence number (0-based).
    pub seq: u32,
    /// Flag bits (SYN, FIN, ACK_REQ).
    pub flags: u8,
    /// Frame payload bytes.
    pub payload: Vec<u8>,
}

/// Errors produced by the framing layer.
#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    #[error("CRC mismatch on frame {seq}: expected {expected:#010x}, got {actual:#010x}")]
    CrcMismatch {
        seq: u32,
        expected: u32,
        actual: u32,
    },
    #[error("bad magic {0:#06x}")]
    BadMagic(u16),
    #[error("payload length {0} exceeds max frame size")]
    PayloadTooLarge(usize),
    #[error("missing frames: {0:?}")]
    MissingFrames(Vec<u32>),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
