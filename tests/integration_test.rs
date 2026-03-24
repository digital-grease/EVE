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
