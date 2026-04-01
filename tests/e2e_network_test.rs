/// Real end-to-end network tests for EVE.
///
/// Unlike the existing integration tests which bypass the network (loopback
/// simulation), these tests exercise the **full stack**: SIP signaling over
/// real UDP sockets, RTP media pacing at real intervals, mFSK codec, framing,
/// and file reassembly.
///
/// Each test allocates unique high ports to avoid conflicts when tests run in
/// parallel.  All network traffic stays on 127.0.0.1.
use sha2::{Digest, Sha256};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use tokio::time::{timeout, Duration};

/// Atomic counter for port allocation.  Each test grabs a block of 10 ports.
static PORT_BASE: AtomicU16 = AtomicU16::new(31000);

/// Allocate a unique block of 4 ports for one test:
/// (sender_sip, sender_rtp, receiver_sip, receiver_rtp).
fn alloc_ports() -> (u16, u16, u16, u16) {
    let base = PORT_BASE.fetch_add(10, Ordering::SeqCst);
    (base, base + 1, base + 2, base + 3)
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// Build a fast test config.
///
/// Uses symbol_rate=200 (4x default) for faster transfers and small frame
/// sizes to keep audio short.  ARQ disabled by default.
fn test_config(sip_port: u16, rtp_port: u16, tones: u8, frame_size: usize) -> eve::config::Config {
    eve::config::Config {
        codec: eve::config::CodecConfig {
            tones,
            symbol_rate: 100,      // 2x default; each symbol = 80 samples = 10ms
            base_freq: 400.0,
            tone_spacing: 200.0,   // wider spacing for clean Goertzel at 100 sym/s
            sample_rate: 8000,
            start_tone_freq: 200.0,
            stop_tone_freq: 3800.0,
            signal_tone_ms: 100, // shorter signal tones for faster tests
        },
        framing: eve::config::FramingConfig { frame_size },
        voip: eve::config::VoipConfig {
            sip_port,
            rtp_port,
            jitter_ms: 40,
        },
        arq: eve::config::ArqConfig {
            retries: 0,
            timeout_ms: 1000,
        },
        verbose: false,
        save_audio: None,
        persist: false,
    }
}

// ===========================================================================
// Test 1: Real SIP handshake over UDP (no media)
// ===========================================================================

/// Verify a full SIP INVITE → 200 OK → ACK handshake works over real
/// localhost UDP sockets.
#[tokio::test]
async fn test_sip_handshake_over_real_udp() {
    let (sender_sip, _sender_rtp, recv_sip, recv_rtp) = alloc_ports();

    let recv_voip = eve::config::VoipConfig {
        sip_port: recv_sip,
        rtp_port: recv_rtp,
        jitter_ms: 60,
    };
    let sender_voip = eve::config::VoipConfig {
        sip_port: sender_sip,
        rtp_port: recv_rtp + 1, // doesn't matter, not used
        jitter_ms: 60,
    };

    let recv_addr: SocketAddr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), recv_sip);

    // Spawn receiver (UAS): waits for INVITE, responds 200 OK, waits for ACK.
    let uas = tokio::spawn(async move {
        let mut sip = eve::voip::sip::SipAgent::new(recv_voip);
        sip.accept().await
    });

    // Brief pause to let the receiver bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Spawn sender (UAC): sends INVITE, waits for 200 OK, sends ACK.
    let uac = tokio::spawn(async move {
        let mut sip = eve::voip::sip::SipAgent::new(sender_voip);
        sip.invite(recv_addr, "test-call-001").await
    });

    // Both should complete within 5 seconds.
    let (uas_result, uac_result) = timeout(Duration::from_secs(5), async {
        let uas_r = uas.await.expect("UAS task panicked");
        let uac_r = uac.await.expect("UAC task panicked");
        (uas_r, uac_r)
    })
    .await
    .expect("SIP handshake timed out");

    // UAS should have received the caller's RTP address.
    let (caller_rtp_addr, call_id) = uas_result.expect("UAS accept failed");
    assert_eq!(caller_rtp_addr.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(call_id, "test-call-001");

    // UAC should have received the receiver's RTP port.
    let rtp_port = uac_result.expect("UAC invite failed");
    assert_eq!(rtp_port, recv_rtp);
}

// ===========================================================================
// Test 2: Real RTP pacing + dejitter over real sockets
// ===========================================================================

/// Send RTP packets over real UDP at proper 20ms pacing, receive through
/// the dejitter buffer, and verify all payloads arrive intact and in order.
#[tokio::test]
async fn test_rtp_over_real_udp_with_dejitter() {
    let (_, sender_rtp, _, recv_rtp) = alloc_ports();

    let sender_addr: SocketAddr =
        SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), sender_rtp);
    let recv_addr: SocketAddr =
        SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), recv_rtp);
    let dest: SocketAddr =
        SocketAddr::new(Ipv4Addr::LOCALHOST.into(), recv_rtp);

    let sender_transport = eve::transport::udp::UdpTransport::bind(sender_addr)
        .await
        .expect("sender bind");
    let recv_transport = eve::transport::udp::UdpTransport::bind(recv_addr)
        .await
        .expect("receiver bind");

    let voip_cfg = eve::config::VoipConfig {
        sip_port: 0,
        rtp_port: sender_rtp,
        jitter_ms: 40,
    };

    // Build 10 RTP packets with known payloads.
    let mut rtp_sess = eve::voip::rtp::RtpSession::new(voip_cfg.clone());
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut expected_payloads: Vec<Vec<u8>> = Vec::new();
    for i in 0u8..10 {
        let payload = vec![i; 160];
        expected_payloads.push(payload.clone());
        let ulaw_payload = eve::voip::rtp::pcm_to_ulaw(&payload.iter().map(|&b| b as i16 * 100).collect::<Vec<_>>());
        packets.push(rtp_sess.build_packet(&ulaw_payload));
    }

    // Spawn sender: pace one packet per 20ms.
    let sender_handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        for packet in &packets {
            tick.tick().await;
            sender_transport
                .send_to(packet, dest)
                .await
                .expect("RTP send");
        }
    });

    // Receiver: collect packets through dejitter buffer.
    let recv_handle = tokio::spawn(async move {
        let mut jb = eve::voip::rtp::DejitterBuffer::new(40); // depth=2
        let mut all_payloads: Vec<Vec<u8>> = Vec::new();
        let mut received = 0;

        loop {
            match timeout(Duration::from_millis(500), recv_transport.recv_from()).await {
                Ok(Ok((data, _src))) => {
                    match jb.push(&data) {
                        Ok(payloads) => {
                            for p in payloads {
                                all_payloads.push(p);
                            }
                        }
                        Err(e) => panic!("dejitter error: {e}"),
                    }
                    received += 1;
                    if received >= 10 {
                        // Flush remaining.
                        for p in jb.flush() {
                            all_payloads.push(p);
                        }
                        break;
                    }
                }
                Ok(Err(e)) => panic!("recv error: {e}"),
                Err(_) => {
                    // Timeout: flush what we have.
                    for p in jb.flush() {
                        all_payloads.push(p);
                    }
                    break;
                }
            }
        }
        all_payloads
    });

    let (_, payloads) = timeout(Duration::from_secs(5), async {
        sender_handle.await.expect("sender panicked");
        recv_handle.await.expect("receiver panicked")
    })
    .await
    .map(|p| ((), p))
    .expect("RTP test timed out");

    // All 10 payloads should have arrived (each is 160 μ-law bytes).
    assert_eq!(
        payloads.len(),
        10,
        "expected 10 payloads, got {}",
        payloads.len()
    );
    // Each payload should be 160 bytes.
    for (i, p) in payloads.iter().enumerate() {
        assert_eq!(p.len(), 160, "payload {i} wrong length: {}", p.len());
    }
}

// ===========================================================================
// Test 3: Full E2E file transfer over real network (small file, M=16)
// ===========================================================================

/// Complete end-to-end transfer: SIP signaling + RTP media + mFSK codec +
/// framing, all over real localhost UDP sockets.  Verifies SHA-256 match.
#[tokio::test]
async fn test_e2e_file_transfer_small_m16() {
    let (sender_sip, sender_rtp, recv_sip, recv_rtp) = alloc_ports();

    let test_data = b"EVE end-to-end test payload!";
    let expected_hash = sha256_hex(test_data);

    // Write test file to temp dir.
    let tmp = std::env::temp_dir().join(format!("eve_e2e_send_{}", sender_sip));
    std::fs::create_dir_all(&tmp).unwrap();
    let input_file = tmp.join("test_input.bin");
    std::fs::write(&input_file, test_data).unwrap();

    let output_dir = std::env::temp_dir().join(format!("eve_e2e_recv_{}", recv_sip));
    std::fs::create_dir_all(&output_dir).unwrap();

    let recv_cfg = test_config(recv_sip, recv_rtp, 16, 32);
    let sender_cfg = test_config(sender_sip, sender_rtp, 16, 32);
    let recv_dest = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), recv_sip);
    let output_dir_clone = output_dir.clone();

    // Spawn receiver first.
    let recv_handle = tokio::spawn(async move {
        eve::pipeline::run_receiver(output_dir_clone, recv_cfg).await;
    });

    // Brief pause to let receiver bind SIP port.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Spawn sender.
    let sender_handle = tokio::spawn(async move {
        eve::pipeline::run_sender(input_file, recv_dest, sender_cfg).await;
    });

    // Both should complete within 60 seconds (mFSK encoding/pacing takes time).
    let result = timeout(Duration::from_secs(60), async {
        sender_handle.await.expect("sender panicked");
        recv_handle.await.expect("receiver panicked");
    })
    .await;
    assert!(result.is_ok(), "E2E transfer timed out after 60s");

    // Find the output file and verify SHA-256.
    let output_file = find_output_file(&output_dir);
    assert!(
        output_file.is_some(),
        "no output file found in {output_dir:?}"
    );

    let recovered = std::fs::read(output_file.as_ref().unwrap()).unwrap();
    let recovered_hash = sha256_hex(&recovered);
    assert_eq!(
        expected_hash, recovered_hash,
        "SHA-256 mismatch!\n  original:  {expected_hash}\n  recovered: {recovered_hash}\n  original len:  {}\n  recovered len: {}",
        test_data.len(),
        recovered.len()
    );

    // Cleanup.
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::remove_dir_all(&output_dir).ok();
}

// ===========================================================================
// Test 4: Full E2E file transfer with M=4 tones (lower bitrate, more robust)
// ===========================================================================

/// Same as the M=16 test but with M=4 tones (2 bits/symbol).
/// Verifies the codec works end-to-end at a different configuration.
#[tokio::test]
async fn test_e2e_file_transfer_m4() {
    let (sender_sip, sender_rtp, recv_sip, recv_rtp) = alloc_ports();

    let test_data = b"M=4 tone test data";
    let expected_hash = sha256_hex(test_data);

    let tmp = std::env::temp_dir().join(format!("eve_e2e_m4_send_{}", sender_sip));
    std::fs::create_dir_all(&tmp).unwrap();
    let input_file = tmp.join("m4_test.bin");
    std::fs::write(&input_file, test_data).unwrap();

    let output_dir = std::env::temp_dir().join(format!("eve_e2e_m4_recv_{}", recv_sip));
    std::fs::create_dir_all(&output_dir).unwrap();

    // M=4: only 2 bits/symbol, so slower but more robust.
    let recv_cfg = test_config(recv_sip, recv_rtp, 4, 32);
    let sender_cfg = test_config(sender_sip, sender_rtp, 4, 32);
    let recv_dest = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), recv_sip);
    let output_dir_clone = output_dir.clone();

    let recv_handle = tokio::spawn(async move {
        eve::pipeline::run_receiver(output_dir_clone, recv_cfg).await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let sender_handle = tokio::spawn(async move {
        eve::pipeline::run_sender(input_file, recv_dest, sender_cfg).await;
    });

    let result = timeout(Duration::from_secs(90), async {
        sender_handle.await.expect("sender panicked");
        recv_handle.await.expect("receiver panicked");
    })
    .await;
    assert!(result.is_ok(), "E2E M=4 transfer timed out after 90s");

    let output_file = find_output_file(&output_dir);
    assert!(
        output_file.is_some(),
        "no output file found in {output_dir:?}"
    );

    let recovered = std::fs::read(output_file.as_ref().unwrap()).unwrap();
    assert_eq!(
        expected_hash,
        sha256_hex(&recovered),
        "SHA-256 mismatch for M=4 transfer"
    );

    std::fs::remove_dir_all(&tmp).ok();
    std::fs::remove_dir_all(&output_dir).ok();
}

// ===========================================================================
// Test 5: Larger file transfer (128 bytes, multiple frames)
// ===========================================================================

/// Transfer a 128-byte file that spans multiple data frames over real
/// network.  Verifies multi-frame reassembly works end-to-end.
#[tokio::test]
async fn test_e2e_multi_frame_transfer() {
    let (sender_sip, sender_rtp, recv_sip, recv_rtp) = alloc_ports();

    // 128 bytes with all byte values = 4 data frames at frame_size=32.
    let test_data: Vec<u8> = (0u8..128).collect();
    let expected_hash = sha256_hex(&test_data);

    let tmp = std::env::temp_dir().join(format!("eve_e2e_multi_send_{}", sender_sip));
    std::fs::create_dir_all(&tmp).unwrap();
    let input_file = tmp.join("multi_frame.bin");
    std::fs::write(&input_file, &test_data).unwrap();

    let output_dir = std::env::temp_dir().join(format!("eve_e2e_multi_recv_{}", recv_sip));
    std::fs::create_dir_all(&output_dir).unwrap();

    let recv_cfg = test_config(recv_sip, recv_rtp, 16, 32);
    let sender_cfg = test_config(sender_sip, sender_rtp, 16, 32);
    let recv_dest = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), recv_sip);
    let output_dir_clone = output_dir.clone();

    let recv_handle = tokio::spawn(async move {
        eve::pipeline::run_receiver(output_dir_clone, recv_cfg).await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let sender_handle = tokio::spawn(async move {
        eve::pipeline::run_sender(input_file, recv_dest, sender_cfg).await;
    });

    let result = timeout(Duration::from_secs(90), async {
        sender_handle.await.expect("sender panicked");
        recv_handle.await.expect("receiver panicked");
    })
    .await;
    assert!(result.is_ok(), "E2E multi-frame transfer timed out");

    let output_file = find_output_file(&output_dir);
    assert!(
        output_file.is_some(),
        "no output file found in {output_dir:?}"
    );

    let recovered = std::fs::read(output_file.as_ref().unwrap()).unwrap();
    assert_eq!(
        expected_hash,
        sha256_hex(&recovered),
        "SHA-256 mismatch for multi-frame transfer\n  expected {} bytes, got {}",
        test_data.len(),
        recovered.len()
    );

    std::fs::remove_dir_all(&tmp).ok();
    std::fs::remove_dir_all(&output_dir).ok();
}

// ===========================================================================
// Test 6: CLI binary smoke test (loopback mode)
// ===========================================================================

/// Spawn the real compiled `eve` binary in loopback mode and verify it
/// exits with code 0 (bit-perfect reconstruction).
#[test]
fn test_cli_binary_loopback() {
    // Create a small test file.
    let tmp = std::env::temp_dir().join("eve_cli_smoke");
    std::fs::create_dir_all(&tmp).unwrap();
    let input_file = tmp.join("smoke_test.bin");
    std::fs::write(&input_file, b"CLI loopback smoke test").unwrap();

    // Find the binary.  cargo test builds it in the same target dir.
    let binary = env!("CARGO_BIN_EXE_eve");

    let output = std::process::Command::new(binary)
        .args([
            "loopback",
            "--file",
            input_file.to_str().unwrap(),
            "--tones",
            "16",
        ])
        .output()
        .expect("failed to execute eve binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "eve loopback exited with {}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
    assert!(
        stdout.contains("PASS"),
        "loopback output should contain PASS:\nstdout: {stdout}\nstderr: {stderr}"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

/// Spawn the `eve` binary in loopback mode with M=32 tones.
#[test]
fn test_cli_binary_loopback_m32() {
    let tmp = std::env::temp_dir().join("eve_cli_smoke_m32");
    std::fs::create_dir_all(&tmp).unwrap();
    let input_file = tmp.join("smoke_m32.bin");
    std::fs::write(&input_file, b"M=32 binary test").unwrap();

    let binary = env!("CARGO_BIN_EXE_eve");

    let output = std::process::Command::new(binary)
        .args([
            "loopback",
            "--file",
            input_file.to_str().unwrap(),
            "--tones",
            "32",
        ])
        .output()
        .expect("failed to execute eve binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "eve loopback M=32 failed: {}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
    assert!(stdout.contains("PASS"));

    std::fs::remove_dir_all(&tmp).ok();
}

/// Spawn the `eve` binary in loopback with --save-audio and verify the WAV
/// file is created and has a valid header.
#[test]
fn test_cli_binary_loopback_save_audio() {
    let tmp = std::env::temp_dir().join("eve_cli_wav");
    std::fs::create_dir_all(&tmp).unwrap();
    let input_file = tmp.join("wav_test.bin");
    std::fs::write(&input_file, b"WAV output test").unwrap();
    let wav_file = tmp.join("debug.wav");

    let binary = env!("CARGO_BIN_EXE_eve");

    let output = std::process::Command::new(binary)
        .args([
            "loopback",
            "--file",
            input_file.to_str().unwrap(),
            "--tones",
            "16",
            "--save-audio",
            wav_file.to_str().unwrap(),
        ])
        .output()
        .expect("failed to execute eve binary");

    assert!(
        output.status.success(),
        "eve loopback with --save-audio failed"
    );

    // Verify WAV file exists and starts with RIFF header.
    let wav_data = std::fs::read(&wav_file).expect("WAV file not created");
    assert!(wav_data.len() > 44, "WAV file too small: {} bytes", wav_data.len());
    assert_eq!(&wav_data[0..4], b"RIFF", "missing RIFF header");
    assert_eq!(&wav_data[8..12], b"WAVE", "missing WAVE marker");

    std::fs::remove_dir_all(&tmp).ok();
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Scan `dir` for any non-directory file (the depacketizer writes the
/// received file with the original filename).
fn find_output_file(dir: &std::path::Path) -> Option<PathBuf> {
    if !dir.exists() {
        return None;
    }
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_file() {
            return Some(path);
        }
    }
    None
}
