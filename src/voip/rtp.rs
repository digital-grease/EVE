/// RTP packetization, G.711 μ-law codec, and dejitter buffer.
///
/// RTP header format follows RFC 3550.
/// G.711 μ-law encoding follows ITU-T G.711 Table 2 (256-entry lookup tables).
use crate::config::VoipConfig;

// ---------------------------------------------------------------------------
// G.711 μ-law codec (ITU-T G.711 / CCITT reference algorithm)
// ---------------------------------------------------------------------------

/// Encode a 16-bit linear PCM sample to an 8-bit μ-law value.
///
/// Implements the standard CCITT G.711 μ-law compression formula:
/// bias = 0x84 (132), exponent derived from leading bit of biased magnitude.
pub fn pcm_to_ulaw_sample(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32767;

    let mut s = sample as i32;

    // Extract sign; work with positive magnitude.
    let sign: u8 = if s < 0 {
        s = -s;
        0x00 // negative → sign bit = 0 in the G.711 convention
    } else {
        0x80 // positive → sign bit = 1
    };

    s = s.min(CLIP) + BIAS;

    // Find the exponent: position of the highest set bit above bit 7 (0x80).
    // For s in [BIAS, CLIP+BIAS] the highest bit is always >= bit 7.
    // exp = (position of highest bit) - 7.
    let highest_bit = 31u32.saturating_sub(s.leading_zeros());
    let exp = (highest_bit as i32 - 7).max(0) as u8;

    let mantissa = ((s >> (exp + 3)) & 0x0F) as u8;
    !(sign | (exp << 4) | mantissa) // bit-complement per G.711
}

/// Decode an 8-bit μ-law value to a 16-bit linear PCM sample.
pub fn ulaw_to_pcm_sample(u: u8) -> i16 {
    const BIAS: i32 = 0x84;

    let u = !u; // undo the bit-complement
    let sign = u & 0x80;
    let exp = ((u >> 4) & 0x07) as i32;
    let mantissa = (u & 0x0F) as i32;

    // Reconstruct magnitude: reverse the exponent shift.
    let magnitude = ((mantissa << 3) + BIAS) << exp;
    let value = magnitude - BIAS;

    if sign != 0 {
        value as i16
    } else {
        -(value as i16)
    }
}

/// Encode a slice of 16-bit PCM samples to μ-law bytes.
pub fn pcm_to_ulaw(samples: &[i16]) -> Vec<u8> {
    samples.iter().map(|&s| pcm_to_ulaw_sample(s)).collect()
}

/// Decode μ-law bytes to 16-bit PCM samples.
pub fn ulaw_to_pcm(ulaw: &[u8]) -> Vec<i16> {
    ulaw.iter().map(|&u| ulaw_to_pcm_sample(u)).collect()
}

// ---------------------------------------------------------------------------
// RTP header (RFC 3550)
// ---------------------------------------------------------------------------

/// Minimum RTP header size in bytes (no CSRC list, no extensions).
const RTP_HEADER_SIZE: usize = 12;

/// Payload type 0 = G.711 μ-law (PCMU), 8 kHz.
const PAYLOAD_TYPE_PCMU: u8 = 0;

/// RTP session state: SSRC, sequence counter, timestamp counter.
pub struct RtpSession {
    pub ssrc: u32,
    seq: u16,
    timestamp: u32,
    _config: VoipConfig,
}

impl RtpSession {
    /// Create a new RTP session with a randomly generated SSRC.
    ///
    /// The SSRC is derived from the current time nanoseconds XOR'd with a
    /// fixed salt (no external rand crate required).
    pub fn new(config: VoipConfig) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0xDEAD_BEEF);
        let ssrc = nanos ^ 0xC0DE_CAFE;
        Self {
            ssrc,
            seq: 0,
            timestamp: 0,
            _config: config,
        }
    }

    /// Build an RTP packet wrapping the given μ-law payload.
    ///
    /// Advances the internal sequence number and timestamp (by 160 samples =
    /// 20 ms at 8 kHz).
    pub fn build_packet(&mut self, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(RTP_HEADER_SIZE + payload.len());

        // Byte 0: V=2, P=0, X=0, CC=0 → 0x80.
        buf.push(0x80);
        // Byte 1: M=0, PT=0 (PCMU).
        buf.push(PAYLOAD_TYPE_PCMU);
        // Bytes 2-3: sequence number.
        buf.extend_from_slice(&self.seq.to_be_bytes());
        // Bytes 4-7: timestamp.
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        // Bytes 8-11: SSRC.
        buf.extend_from_slice(&self.ssrc.to_be_bytes());
        // Payload.
        buf.extend_from_slice(payload);

        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(160);

        buf
    }

    /// Parse an RTP packet, returning `(sequence, timestamp, payload)`.
    pub fn parse_packet(data: &[u8]) -> Result<(u16, u32, &[u8]), super::VoipError> {
        if data.len() < RTP_HEADER_SIZE {
            return Err(super::VoipError::Rtp(format!(
                "packet too short: {} bytes",
                data.len()
            )));
        }
        let version = (data[0] >> 6) & 0x03;
        if version != 2 {
            return Err(super::VoipError::Rtp(format!("bad RTP version {version}")));
        }
        let seq = u16::from_be_bytes([data[2], data[3]]);
        let ts = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let payload = &data[RTP_HEADER_SIZE..];
        Ok((seq, ts, payload))
    }
}

// ---------------------------------------------------------------------------
// Dejitter buffer
// ---------------------------------------------------------------------------

/// Packet stored in the dejitter buffer.
struct Buffered {
    seq: u16,
    timestamp: u32,
    payload: Vec<u8>,
}

/// Simple dejitter buffer: holds packets for `depth_packets` packets before
/// releasing them in order to the decoder.
pub struct DejitterBuffer {
    depth: usize,
    buffer: std::collections::BTreeMap<u16, Buffered>,
    next_seq: Option<u16>,
}

impl DejitterBuffer {
    /// Create a new dejitter buffer.
    ///
    /// `jitter_ms` at 8 kHz with 160 samples/packet → depth = jitter_ms / 20.
    pub fn new(jitter_ms: u32) -> Self {
        let depth = jitter_ms.div_ceil(20).max(1) as usize;
        Self {
            depth,
            buffer: std::collections::BTreeMap::new(),
            next_seq: None,
        }
    }

    /// Push a raw RTP packet into the buffer.
    ///
    /// Returns a list of in-order payloads that are ready to be decoded.
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>, super::VoipError> {
        let (seq, ts, payload) = RtpSession::parse_packet(data)?;
        self.buffer.insert(
            seq,
            Buffered {
                seq,
                timestamp: ts,
                payload: payload.to_vec(),
            },
        );
        if self.next_seq.is_none() {
            self.next_seq = Some(seq);
        }
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<Vec<u8>>, super::VoipError> {
        let mut out = Vec::new();
        while self.buffer.len() >= self.depth {
            let next = match self.next_seq {
                Some(s) => s,
                None => break,
            };
            match self.buffer.remove(&next) {
                Some(b) => {
                    out.push(b.payload);
                    self.next_seq = Some(next.wrapping_add(1));
                }
                None => {
                    // Gap: skip and advance.
                    self.next_seq = Some(next.wrapping_add(1));
                }
            }
        }
        Ok(out)
    }

    /// Flush remaining packets (call at end of stream).
    pub fn flush(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for (_, b) in std::mem::take(&mut self.buffer) {
            out.push(b.payload);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VoipConfig;

    fn voip_cfg() -> VoipConfig {
        VoipConfig::default()
    }

    // ---- μ-law round-trip ----

    #[test]
    fn test_ulaw_silence() {
        // 0 should encode and decode back to approximately 0.
        let encoded = pcm_to_ulaw_sample(0);
        let decoded = ulaw_to_pcm_sample(encoded);
        assert!(decoded.abs() < 100, "decoded silence too large: {decoded}");
    }

    #[test]
    fn test_ulaw_roundtrip_accuracy() {
        // G.711 μ-law has about 8 bits of dynamic resolution; allow ±128 LSB error.
        let test_values: &[i16] = &[0, 100, 1000, 5000, 10000, 30000, -100, -5000, -30000];
        for &v in test_values {
            let u = pcm_to_ulaw_sample(v);
            let d = ulaw_to_pcm_sample(u);
            let err = (v as i32 - d as i32).abs();
            assert!(
                err < 256,
                "ulaw round-trip error {err} for input {v} (decoded {d})"
            );
        }
    }

    #[test]
    fn test_ulaw_encode_decode_vec() {
        let pcm: Vec<i16> = (0..160).map(|i| (i * 200 - 16000) as i16).collect();
        let ulaw = pcm_to_ulaw(&pcm);
        let recovered = ulaw_to_pcm(&ulaw);
        assert_eq!(ulaw.len(), 160);
        assert_eq!(recovered.len(), 160);
    }

    // ---- RTP header ----

    #[test]
    fn test_rtp_build_parse() {
        let mut sess = RtpSession::new(voip_cfg());
        let payload = vec![0xAAu8; 160];
        let packet = sess.build_packet(&payload);
        let (seq, ts, pl) = RtpSession::parse_packet(&packet).unwrap();
        assert_eq!(seq, 0);
        assert_eq!(ts, 0);
        assert_eq!(pl, payload.as_slice());
    }

    #[test]
    fn test_rtp_sequence_and_timestamp_advance() {
        let mut sess = RtpSession::new(voip_cfg());
        let _ = sess.build_packet(&[0u8; 160]);
        let packet2 = sess.build_packet(&[0u8; 160]);
        let (seq, ts, _) = RtpSession::parse_packet(&packet2).unwrap();
        assert_eq!(seq, 1);
        assert_eq!(ts, 160);
    }

    #[test]
    fn test_rtp_bad_version() {
        let mut bad = vec![0x00u8; 12]; // version bits = 0
        bad[0] = 0x40; // version = 1
        assert!(RtpSession::parse_packet(&bad).is_err());
    }

    // ---- Dejitter buffer ----

    #[test]
    fn test_dejitter_in_order() {
        let mut sess = RtpSession::new(voip_cfg());
        let mut jb = DejitterBuffer::new(60); // depth=3

        let p0 = sess.build_packet(&[0u8; 160]);
        let p1 = sess.build_packet(&[1u8; 160]);
        let p2 = sess.build_packet(&[2u8; 160]);

        assert!(jb.push(&p0).unwrap().is_empty()); // depth not reached
        assert!(jb.push(&p1).unwrap().is_empty());
        // Third packet fills depth → drains.
        let out = jb.push(&p2).unwrap();
        assert!(!out.is_empty());
    }

    #[test]
    fn test_dejitter_flush() {
        let mut sess = RtpSession::new(voip_cfg());
        let mut jb = DejitterBuffer::new(60);
        let p0 = sess.build_packet(&[0u8; 160]);
        jb.push(&p0).unwrap();
        let flushed = jb.flush();
        assert_eq!(flushed.len(), 1);
    }
}
