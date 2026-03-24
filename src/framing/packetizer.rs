/// Packetizer: splits a file into a sequence of [`Frame`]s.
use super::{flags, Frame, FramingError, MAGIC};
use crate::config::FramingConfig;
use crc32fast::Hasher;

/// Splits raw file data into a [`Vec<Frame>`] ready for mFSK encoding.
pub struct Packetizer {
    config: FramingConfig,
}

impl Packetizer {
    /// Create a new packetizer with the given configuration.
    pub fn new(config: FramingConfig) -> Self {
        Self { config }
    }

    /// Packetize `data` (file contents) with the given `filename`.
    ///
    /// The first frame carries a JSON SYN payload with `filename` and total size.
    /// Remaining frames carry raw data chunks.  The last frame has `FIN` set.
    pub fn packetize(&self, data: &[u8], filename: &str) -> Result<Vec<Frame>, FramingError> {
        let max_payload = self.config.frame_size;
        let mut frames: Vec<Frame> = Vec::new();
        let mut seq: u32 = 0;

        // SYN frame: metadata as JSON.
        // SYN payload is not subject to the data frame_size limit because it
        // carries fixed metadata whose size depends on the filename length.
        let syn_payload = format!(
            r#"{{"filename":"{}","size":{}}}"#,
            escape_json_string(filename),
            data.len()
        )
        .into_bytes();
        frames.push(Frame {
            seq,
            flags: flags::SYN,
            payload: syn_payload,
        });
        seq += 1;

        // Data frames.
        let chunks: Vec<&[u8]> = data.chunks(max_payload).collect();
        let n = chunks.len();
        for (i, chunk) in chunks.into_iter().enumerate() {
            let f = flags::SYN; // clear SYN for data frames
            let flag_byte = if i + 1 == n { flags::FIN } else { 0u8 };
            let _ = f; // suppress warning
            frames.push(Frame {
                seq,
                flags: flag_byte,
                payload: chunk.to_vec(),
            });
            seq += 1;
        }

        // If the input was empty there are no data frames; mark the SYN frame FIN too.
        if data.is_empty() {
            frames[0].flags |= flags::FIN;
        }

        Ok(frames)
    }
}

/// Serialize a [`Frame`] to bytes (big-endian wire format).
///
/// Layout:
/// ```text
/// Magic(2) | SeqNum(4) | Flags(1) | PayloadLen(2) | Payload(N) | CRC-32(4)
/// ```
pub fn serialize_frame(frame: &Frame) -> Vec<u8> {
    let payload_len = frame.payload.len() as u16;
    let header_size = 2 + 4 + 1 + 2; // magic + seq + flags + payload_len
    let total = header_size + frame.payload.len() + 4;
    let mut buf = Vec::with_capacity(total);

    buf.extend_from_slice(&MAGIC.to_be_bytes());
    buf.extend_from_slice(&frame.seq.to_be_bytes());
    buf.push(frame.flags);
    buf.extend_from_slice(&payload_len.to_be_bytes());
    buf.extend_from_slice(&frame.payload);

    let mut hasher = Hasher::new();
    hasher.update(&buf);
    let crc = hasher.finalize();
    buf.extend_from_slice(&crc.to_be_bytes());

    buf
}

/// Deserialize a [`Frame`] from bytes, verifying the CRC.
pub fn deserialize_frame(data: &[u8]) -> Result<Frame, FramingError> {
    const HEADER_SIZE: usize = 2 + 4 + 1 + 2;
    const CRC_SIZE: usize = 4;
    const MIN_SIZE: usize = HEADER_SIZE + CRC_SIZE;

    if data.len() < MIN_SIZE {
        return Err(FramingError::BadMagic(0));
    }

    // Verify magic.
    let magic = u16::from_be_bytes([data[0], data[1]]);
    if magic != MAGIC {
        return Err(FramingError::BadMagic(magic));
    }

    let seq = u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
    let flag_byte = data[6];
    let payload_len = u16::from_be_bytes([data[7], data[8]]) as usize;

    if data.len() < HEADER_SIZE + payload_len + CRC_SIZE {
        return Err(FramingError::BadMagic(magic));
    }

    let payload = data[HEADER_SIZE..HEADER_SIZE + payload_len].to_vec();

    // Verify CRC over everything before the CRC field.
    let crc_offset = HEADER_SIZE + payload_len;
    let expected_crc = u32::from_be_bytes([
        data[crc_offset],
        data[crc_offset + 1],
        data[crc_offset + 2],
        data[crc_offset + 3],
    ]);
    let mut hasher = Hasher::new();
    hasher.update(&data[..crc_offset]);
    let actual_crc = hasher.finalize();

    if actual_crc != expected_crc {
        return Err(FramingError::CrcMismatch {
            seq,
            expected: expected_crc,
            actual: actual_crc,
        });
    }

    Ok(Frame {
        seq,
        flags: flag_byte,
        payload,
    })
}

/// Split a contiguous byte buffer (output of the mFSK decoder) into individual
/// [`Frame`]s by reading successive frame headers.
///
/// Trailing zero-padding (symbol-boundary padding from the encoder) is
/// silently ignored once no valid magic bytes are found.
pub fn split_frames(data: &[u8]) -> Result<Vec<Frame>, FramingError> {
    const HEADER_SIZE: usize = 2 + 4 + 1 + 2; // magic + seq + flags + payload_len
    const CRC_SIZE: usize = 4;
    const MIN_FRAME: usize = HEADER_SIZE + CRC_SIZE;

    let mut frames = Vec::new();
    let mut pos = 0;

    while pos + MIN_FRAME <= data.len() {
        // Check magic; stop on padding or garbage.
        let magic = u16::from_be_bytes([data[pos], data[pos + 1]]);
        if magic != MAGIC {
            break;
        }

        let payload_len = u16::from_be_bytes([data[pos + 7], data[pos + 8]]) as usize;
        let frame_end = pos + HEADER_SIZE + payload_len + CRC_SIZE;
        if frame_end > data.len() {
            break; // truncated — treat as end of stream
        }

        let frame = deserialize_frame(&data[pos..frame_end])?;
        frames.push(frame);
        pos = frame_end;
    }

    Ok(frames)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Escape a string for embedding in a JSON value (handles `"` and `\`).
fn escape_json_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FramingConfig;
    use crate::framing::flags;

    fn cfg(frame_size: usize) -> FramingConfig {
        FramingConfig { frame_size }
    }

    #[test]
    fn test_syn_first_fin_last() {
        let p = Packetizer::new(cfg(128));
        let data = b"hello world";
        let frames = p.packetize(data, "test.txt").unwrap();
        assert!(
            frames[0].flags & flags::SYN != 0,
            "first frame must have SYN"
        );
        assert!(
            frames.last().unwrap().flags & flags::FIN != 0,
            "last frame must have FIN"
        );
    }

    #[test]
    fn test_seq_numbers_increment() {
        let p = Packetizer::new(cfg(4));
        let frames = p.packetize(b"abcdefghij", "t").unwrap();
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.seq, i as u32);
        }
    }

    #[test]
    fn test_serialize_deserialize() {
        let frame = Frame {
            seq: 42,
            flags: flags::FIN,
            payload: b"test payload".to_vec(),
        };
        let bytes = serialize_frame(&frame);
        let recovered = deserialize_frame(&bytes).unwrap();
        assert_eq!(recovered.seq, 42);
        assert_eq!(recovered.flags, flags::FIN);
        assert_eq!(recovered.payload, b"test payload");
    }

    #[test]
    fn test_crc_corruption_detected() {
        let frame = Frame {
            seq: 1,
            flags: 0,
            payload: b"data".to_vec(),
        };
        let mut bytes = serialize_frame(&frame);
        // Flip a bit in the payload.
        let payload_offset = 2 + 4 + 1 + 2;
        bytes[payload_offset] ^= 0xFF;
        assert!(matches!(
            deserialize_frame(&bytes),
            Err(FramingError::CrcMismatch { .. })
        ));
    }

    #[test]
    fn test_empty_data_syn_fin() {
        let p = Packetizer::new(cfg(128));
        let frames = p.packetize(b"", "empty.bin").unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].flags & flags::SYN != 0);
        assert!(frames[0].flags & flags::FIN != 0);
    }

    #[test]
    fn test_split_frames_roundtrip() {
        let p = Packetizer::new(cfg(8));
        let frames = p.packetize(b"abcdefghij", "t").unwrap();

        // Serialise all frames into one buffer.
        let mut buf: Vec<u8> = Vec::new();
        for f in &frames {
            buf.extend_from_slice(&serialize_frame(f));
        }

        // Split should recover the same number of frames with identical content.
        let recovered = split_frames(&buf).unwrap();
        assert_eq!(recovered.len(), frames.len());
        for (a, b) in frames.iter().zip(recovered.iter()) {
            assert_eq!(a.seq, b.seq);
            assert_eq!(a.flags, b.flags);
            assert_eq!(a.payload, b.payload);
        }
    }

    #[test]
    fn test_split_frames_ignores_trailing_padding() {
        let frame = Frame {
            seq: 0,
            flags: flags::SYN | flags::FIN,
            payload: b"x".to_vec(),
        };
        let mut buf = serialize_frame(&frame);
        buf.extend_from_slice(&[0u8; 32]); // trailing zeros (symbol padding)
        let recovered = split_frames(&buf).unwrap();
        assert_eq!(recovered.len(), 1);
    }
}
