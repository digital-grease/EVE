/// Integration test: full codec + framing round-trip (no network).
///
/// Simulates the sender and receiver pipelines on localhost:
/// - Sender: packetize → serialize all frames → mFSK encode → μ-law
/// - Receiver: μ-law → mFSK decode → split_frames → depacketize → file
///
/// Asserts SHA-256 hash equality between original and recovered data.
use sha2::{Digest, Sha256};

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// End-to-end codec + framing round-trip across all supported tone counts.
#[test]
fn test_loopback_sha256_all_tone_counts() {
    let test_data = b"Hello from eve integration test! \
        This payload is long enough to span multiple frames and symbols.";

    for &tones in &[2u8, 4, 8, 16, 32] {
        let recovered = pipeline_roundtrip(test_data, tones, 64);
        assert_eq!(
            sha256_hex(test_data),
            sha256_hex(&recovered),
            "SHA-256 mismatch at M={tones}"
        );
    }
}

/// Multi-frame file transfer: 512 bytes across 8 data frames.
#[test]
fn test_multi_frame_roundtrip() {
    let data: Vec<u8> = (0u8..=255).cycle().take(512).collect();
    let recovered = pipeline_roundtrip(&data, 16, 64);
    assert_eq!(data, recovered, "multi-frame round-trip failed");
}

/// Single-frame edge case: payload fits entirely in the SYN+FIN frame.
#[test]
fn test_empty_file_roundtrip() {
    let recovered = pipeline_roundtrip(b"", 16, 128);
    assert_eq!(recovered, b"");
}

// ---------------------------------------------------------------------------
// ARQ tests
// ---------------------------------------------------------------------------

/// NAK frame serialization / deserialization round-trip.
#[test]
fn test_nak_frame_roundtrip() {
    use eve::framing::flags;
    use eve::framing::packetizer::{deserialize_nak_frame, serialize_nak_frame};

    let missing: Vec<u32> = vec![3, 7, 15, 42];
    let bytes = serialize_nak_frame(&missing);

    // NAK flag must be set.
    // Layout: Magic(2) + Seq(4) + Flags(1) + PayloadLen(2) + Payload(N) + CRC(4)
    assert_eq!(bytes[6], flags::NAK, "NAK flag must be set in serialized frame");

    let recovered = deserialize_nak_frame(&bytes).expect("deserialize NAK frame");
    assert_eq!(recovered, missing, "NAK seq list did not round-trip");
}

/// Simulated ARQ recovery: send all frames, drop some, verify the receiver can
/// identify missing seqs, the sender re-encodes them, and the receiver
/// reassembles the complete file after the retransmission.
///
/// This exercises the full framing + codec + depacketizer path.
/// No network/SIP/RTP is involved.
#[test]
fn test_arq_recovery_with_simulated_loss() {
    use eve::codec::{MfskDecoder, MfskEncoder};
    use eve::config::{CodecConfig, FramingConfig};
    use eve::framing::depacketizer::Depacketizer;
    use eve::framing::packetizer::{
        deserialize_nak_frame, serialize_frame, serialize_nak_frame, split_frames, Packetizer,
    };
    use eve::voip::rtp::{pcm_to_ulaw, ulaw_to_pcm};
    use sha2::{Digest, Sha256};

    let original: Vec<u8> = (0u8..=255).cycle().take(640).collect();
    let cc = CodecConfig {
        tones: 16,
        ..CodecConfig::default()
    };
    let fc = FramingConfig { frame_size: 64 };

    let packetizer = Packetizer::new(fc);
    let frames = packetizer.packetize(&original, "arq_test.bin").unwrap();
    assert!(frames.len() > 4, "need enough frames to simulate loss");

    // ---- Sender: build per-seq byte map ----
    let mut frame_bytes: std::collections::HashMap<u32, Vec<u8>> =
        std::collections::HashMap::new();
    let mut all_bytes: Vec<u8> = Vec::new();
    for frame in &frames {
        let serialized = serialize_frame(frame);
        frame_bytes.insert(frame.seq, serialized.clone());
        all_bytes.extend_from_slice(&serialized);
    }

    let enc = MfskEncoder::new(cc.clone()).unwrap();
    let samples = enc.encode(&all_bytes);
    let ulaw = pcm_to_ulaw(&samples);
    let pcm = ulaw_to_pcm(&ulaw);

    // ---- Receiver pass 1: intentionally omit every third data frame ----
    let dec = MfskDecoder::new(cc.clone()).unwrap();
    let decoded_bytes = dec.decode(&pcm).unwrap();
    let all_frames = split_frames(&decoded_bytes).unwrap();

    let out_dir = std::env::temp_dir().join(format!(
        "eve_arq_test_{:?}",
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut depack = Depacketizer::new(out_dir.clone());

    // Push all frames except every third data frame (seq % 3 == 0, skip seq 0 which is SYN).
    let mut completed = false;
    for frame in &all_frames {
        let drop = frame.seq > 0 && frame.seq % 3 == 0;
        if !drop {
            if let Ok(Some(_)) = depack.push(frame.clone()) {
                completed = true;
                break;
            }
        }
    }

    // With some frames dropped, transfer should not be complete.
    assert!(!completed, "transfer should not complete with missing frames");
    let missing = depack.missing_seqs();
    assert!(!missing.is_empty(), "should have missing seqs after pass 1");

    // ---- Receiver: build NAK, sender decodes it ----
    let nak_bytes = serialize_nak_frame(&missing);
    let recovered_missing = deserialize_nak_frame(&nak_bytes).unwrap();
    assert_eq!(recovered_missing, missing, "NAK seq list mismatch");

    // ---- Sender: retransmit only missing frames ----
    let retry_bytes: Vec<u8> = missing
        .iter()
        .filter_map(|&seq| frame_bytes.get(&seq))
        .flat_map(|b| b.iter().copied())
        .collect();

    let enc2 = MfskEncoder::new(cc.clone()).unwrap();
    let retry_samples = enc2.encode(&retry_bytes);
    let retry_ulaw = pcm_to_ulaw(&retry_samples);
    let retry_pcm = ulaw_to_pcm(&retry_ulaw);

    // ---- Receiver pass 2: decode retransmission ----
    let dec2 = MfskDecoder::new(cc).unwrap();
    let retry_decoded = dec2.decode(&retry_pcm).unwrap();
    let retry_frames = split_frames(&retry_decoded).unwrap();

    let mut final_path = None;
    for frame in retry_frames {
        if let Ok(Some(path)) = depack.push(frame) {
            final_path = Some(path);
            break;
        }
    }

    let path = final_path.expect("transfer should complete after ARQ retransmission");
    let recovered = std::fs::read(&path).unwrap();
    std::fs::remove_dir_all(&out_dir).ok();

    // Assert SHA-256 match — proves bit-perfect recovery.
    let orig_hash = format!("{:x}", Sha256::new().chain_update(&original).finalize());
    let recv_hash = format!("{:x}", Sha256::new().chain_update(&recovered).finalize());
    assert_eq!(orig_hash, recv_hash, "ARQ recovery produced wrong file content");

    // Assert ARQ was actually exercised (retry_frames had frames to push).
    assert!(
        !missing.is_empty(),
        "test must have exercised at least one retransmission"
    );
}

// ---------------------------------------------------------------------------
// Shared pipeline simulation
// ---------------------------------------------------------------------------

/// Simulate the full sender → receiver pipeline without a network.
///
/// Mirrors the real pipeline exactly:
/// - serialize all frames into one buffer
/// - one mFSK encode
/// - μ-law encode/decode (simulating RTP transport with no packet loss)
/// - one mFSK decode
/// - split_frames
/// - depacketize
fn pipeline_roundtrip(data: &[u8], tones: u8, frame_size: usize) -> Vec<u8> {
    use eve::codec::{MfskDecoder, MfskEncoder};
    use eve::config::{CodecConfig, FramingConfig};
    use eve::framing::depacketizer::Depacketizer;
    use eve::framing::packetizer::{serialize_frame, split_frames, Packetizer};
    use eve::voip::rtp::{pcm_to_ulaw, ulaw_to_pcm};

    let cc = CodecConfig {
        tones,
        ..CodecConfig::default()
    };
    let fc = FramingConfig { frame_size };

    // ---- Sender side ----
    let packetizer = Packetizer::new(fc);
    let frames = packetizer.packetize(data, "test.bin").expect("packetize");

    let mut all_bytes: Vec<u8> = Vec::new();
    for frame in &frames {
        all_bytes.extend_from_slice(&serialize_frame(frame));
    }

    let enc = MfskEncoder::new(cc.clone()).expect("encoder");
    let samples = enc.encode(&all_bytes);

    // Simulate RTP transport: PCM → μ-law → PCM (lossy but deterministic).
    let ulaw = pcm_to_ulaw(&samples);
    let pcm = ulaw_to_pcm(&ulaw);

    // ---- Receiver side ----
    let dec = MfskDecoder::new(cc).expect("decoder");
    let decoded_bytes = dec.decode(&pcm).expect("mFSK decode");

    let recovered_frames = split_frames(&decoded_bytes).expect("split_frames");
    assert!(
        !recovered_frames.is_empty(),
        "no frames recovered at M={tones}"
    );

    let out_dir = {
        let p = std::env::temp_dir().join(format!(
            "eve_integ_{}_{}_{:?}",
            tones,
            frame_size,
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    };

    let mut depack = Depacketizer::new(out_dir.clone());
    for frame in recovered_frames {
        if let Some(path) = depack.push(frame).expect("depack push") {
            let result = std::fs::read(&path).expect("read output");
            std::fs::remove_dir_all(&out_dir).ok();
            return result;
        }
    }

    std::fs::remove_dir_all(&out_dir).ok();
    panic!("transfer never completed at M={tones}");
}
