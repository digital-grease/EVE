/// Async pipeline tying framing → codec → VoIP together.
use crate::codec::{MfskDecoder, MfskEncoder};
use crate::config::Config;
use crate::framing::depacketizer::Depacketizer;
use crate::framing::packetizer::{serialize_frame, split_frames, Packetizer};
use crate::voip::rtp::{pcm_to_ulaw, ulaw_to_pcm, DejitterBuffer, RtpSession};
use crate::voip::sip::SipAgent;
use crate::wav;
use std::io::BufWriter;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, Duration, MissedTickBehavior};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Sender pipeline
// ---------------------------------------------------------------------------

/// Run the sender pipeline: file → frames → mFSK PCM → RTP → UDP.
///
/// All frames are serialised into a single byte buffer and encoded as one
/// continuous mFSK audio stream, so the receiver can decode with one pass.
pub async fn run_sender(file: PathBuf, dest: SocketAddr, config: Config) {
    // ---- SIP signalling ----
    let sip = SipAgent::new(config.voip.clone());
    let call_id = format!("eve-{}", std::process::id());
    info!("sending INVITE to {dest}");
    let receiver_rtp_port = match sip.invite(dest, &call_id).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SIP error: {e}");
            return;
        }
    };
    let rtp_dest: SocketAddr = SocketAddr::new(dest.ip(), receiver_rtp_port);
    info!("call established, receiver RTP at {rtp_dest}");

    // ---- Encode (CPU-bound, run in blocking thread) ----
    let cfg_clone = config.clone();
    let file_clone = file.clone();
    let encode_result = tokio::task::spawn_blocking(move || {
        let data = std::fs::read(&file_clone)?;
        let filename = file_clone
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("data")
            .to_owned();

        let packetizer = Packetizer::new(cfg_clone.framing.clone());
        let frames = packetizer
            .packetize(&data, &filename)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Serialise ALL frames into one contiguous byte buffer, then encode
        // the whole thing as a single mFSK audio stream.
        let mut all_bytes: Vec<u8> = Vec::new();
        for frame in &frames {
            all_bytes.extend_from_slice(&serialize_frame(frame));
        }

        let enc = MfskEncoder::new(cfg_clone.codec.clone())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;

        let samples = enc.encode(&all_bytes);
        Ok::<_, std::io::Error>(samples)
    })
    .await
    .expect("blocking task panicked");

    let samples = match encode_result {
        Ok(s) => s,
        Err(e) => {
            eprintln!("encode error: {e}");
            return;
        }
    };

    if samples.is_empty() {
        eprintln!("nothing to send");
        return;
    }

    // Optionally save PCM to WAV.
    if let Some(ref wav_path) = config.save_audio {
        if let Ok(f) = std::fs::File::create(wav_path) {
            let mut bw = BufWriter::new(f);
            let _ = wav::write_wav(&mut bw, config.codec.sample_rate, &samples);
        }
    }

    // ---- RTP send ----
    let rtp_local: SocketAddr = SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        config.voip.rtp_port,
    );
    let transport = match crate::transport::udp::UdpTransport::bind(rtp_local).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("RTP bind error: {e}");
            return;
        }
    };

    let mut rtp_sess = RtpSession::new(config.voip.clone());

    // Pace packets at one 160-sample chunk per 20 ms (8 kHz / 160 = 50 pps).
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);

    // Producer: convert the single PCM stream to μ-law chunks.
    tokio::spawn(async move {
        for chunk in samples.chunks(160) {
            let ulaw = pcm_to_ulaw(chunk);
            if tx.send(ulaw).await.is_err() {
                break;
            }
        }
        // tx dropped → rx.recv() returns None → sender loop exits
    });

    // Consumer: pace and send.
    let mut sent = 0usize;
    while let Some(ulaw) = rx.recv().await {
        tick.tick().await;
        let packet = rtp_sess.build_packet(&ulaw);
        if let Err(e) = transport.send_to(&packet, rtp_dest).await {
            warn!("RTP send error: {e}");
        }
        sent += 1;
        if config.verbose && sent % 50 == 0 {
            info!("sent {sent} RTP packets");
        }
    }

    info!("transfer complete: {sent} RTP packets sent");

    // ---- BYE ----
    if let Err(e) = sip.bye(dest, &call_id).await {
        warn!("BYE error: {e}");
    }
}

// ---------------------------------------------------------------------------
// Receiver pipeline
// ---------------------------------------------------------------------------

/// Run the receiver pipeline: UDP → RTP → PCM → mFSK → frames → file.
///
/// When `config.persist` is true the pipeline loops: after each completed (or
/// failed) transfer it prints a status line and immediately re-listens for the
/// next INVITE. Press Ctrl-C to stop.
pub async fn run_receiver(output_dir: PathBuf, config: Config) {
    loop {
        receive_one(&output_dir, &config).await;
        if !config.persist {
            break;
        }
        info!("persist mode — waiting for next call");
    }
}

/// Handle a single SIP call: accept → collect RTP → decode → write file.
async fn receive_one(output_dir: &PathBuf, config: &Config) {
    // Clone the fields we need to move into spawned tasks up front.
    let voip = config.voip.clone();
    let codec = config.codec.clone();

    // ---- SIP signalling ----
    let sip = SipAgent::new(voip.clone());
    info!("listening for INVITE on :{}", voip.sip_port);
    let (caller_rtp_addr, call_id) = match sip.accept().await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("SIP accept error: {e}");
            return;
        }
    };
    info!("call accepted, caller RTP at {caller_rtp_addr}");

    // ---- RTP receive ----
    let rtp_local: SocketAddr = SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        voip.rtp_port,
    );
    let transport = match crate::transport::udp::UdpTransport::bind(rtp_local).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("RTP bind error: {e}");
            return;
        }
    };

    // BYE waiter sends a oneshot when the call ends.
    let (bye_tx, bye_rx) = oneshot::channel::<()>();
    let sip_bye = SipAgent::new(voip.clone());
    let call_id_for_bye = call_id.clone();
    tokio::spawn(async move {
        let _ = sip_bye.wait_for_bye(&call_id_for_bye).await;
        let _ = bye_tx.send(());
    });

    // μ-law payload channel: RTP loop → collector.
    let (ulaw_tx, mut ulaw_rx) = mpsc::channel::<Vec<u8>>(256);

    // RTP receive loop: exits when BYE fires, flushes dejitter buffer first.
    let jitter_ms = voip.jitter_ms;
    tokio::spawn(async move {
        let mut jb = DejitterBuffer::new(jitter_ms);
        let mut bye_rx = bye_rx;

        loop {
            tokio::select! {
                // BYE received — flush remaining buffered packets and stop.
                _ = &mut bye_rx => {
                    for p in jb.flush() {
                        if ulaw_tx.send(p).await.is_err() { break; }
                    }
                    break; // drops ulaw_tx → ulaw_rx.recv() returns None
                }

                result = transport.recv_from() => {
                    match result {
                        Ok((data, _src)) => {
                            match jb.push(&data) {
                                Ok(payloads) => {
                                    for p in payloads {
                                        if ulaw_tx.send(p).await.is_err() { return; }
                                    }
                                }
                                Err(e) => warn!("dejitter error: {e}"),
                            }
                        }
                        Err(e) => {
                            warn!("UDP recv error: {e}");
                            break;
                        }
                    }
                }
            }
        }
        // ulaw_tx dropped here → collector loop exits cleanly
    });

    // Collect all μ-law payloads (terminates when ulaw_tx is dropped above).
    let mut all_ulaw: Vec<u8> = Vec::new();
    while let Some(payload) = ulaw_rx.recv().await {
        all_ulaw.extend_from_slice(&payload);
    }

    info!("RTP stream ended, {} μ-law bytes collected", all_ulaw.len());

    // ---- Decode (blocking) ----
    let output_dir_clone = output_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        // μ-law → PCM → mFSK decode → raw bytes (all frames concatenated).
        let pcm = ulaw_to_pcm(&all_ulaw);

        let dec = MfskDecoder::new(codec)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;

        let all_bytes = dec
            .decode(&pcm)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Split the byte stream back into individual frames.
        let frames = split_frames(&all_bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        if frames.is_empty() {
            return Err(std::io::Error::other("no valid frames decoded"));
        }

        // Reassemble through the depacketizer.
        let mut depack = Depacketizer::new(output_dir_clone);
        let mut output_path = None;
        for frame in frames {
            match depack.push(frame) {
                Ok(Some(path)) => {
                    output_path = Some(path);
                    break; // file complete
                }
                Ok(None) => {} // waiting for more frames
                Err(e) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()));
                }
            }
        }

        Ok(output_path)
    })
    .await
    .expect("blocking task panicked");

    match result {
        Ok(Some(path)) => info!("file written to {path:?}"),
        Ok(None) => warn!("incomplete transfer — missing frames"),
        Err(e) => eprintln!("decode error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Loopback mode
// ---------------------------------------------------------------------------

/// Run a local loopback test: encode then decode without network.
///
/// Prints bit-error-rate (BER) and whether reconstruction is bit-perfect.
/// Returns `true` on bit-perfect reconstruction.
pub fn run_loopback(file: PathBuf, config: Config) -> bool {
    let data = match std::fs::read(&file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error reading {:?}: {e}", file);
            return false;
        }
    };

    let enc = match MfskEncoder::new(config.codec.clone()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("encoder error: {e}");
            return false;
        }
    };
    let samples = enc.encode(&data);

    if let Some(ref wav_path) = config.save_audio {
        match std::fs::File::create(wav_path) {
            Ok(f) => {
                let mut bw = BufWriter::new(f);
                if let Err(e) = wav::write_wav(&mut bw, config.codec.sample_rate, &samples) {
                    eprintln!("warning: failed to write WAV: {e}");
                }
            }
            Err(e) => eprintln!("warning: cannot create WAV file: {e}"),
        }
    }

    let dec = match MfskDecoder::new(config.codec.clone()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("decoder error: {e}");
            return false;
        }
    };
    let decoded = match dec.decode(&samples) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("decode error: {e}");
            return false;
        }
    };

    let original = data.as_slice();
    let recovered = if decoded.len() >= original.len() {
        &decoded[..original.len()]
    } else {
        decoded.as_slice()
    };

    let total_bits = original.len() * 8;
    let error_bits: usize = original
        .iter()
        .zip(recovered.iter())
        .map(|(&a, &b)| (a ^ b).count_ones() as usize)
        .sum::<usize>()
        + (original.len().saturating_sub(recovered.len())) * 8;

    let ber = if total_bits > 0 {
        error_bits as f64 / total_bits as f64
    } else {
        0.0
    };

    println!("loopback: {total_bits} bits, {error_bits} errors, BER = {ber:.6}");

    if config.verbose {
        println!(
            "  original  ({} bytes): {:?}",
            original.len(),
            &original[..original.len().min(16)]
        );
        println!(
            "  recovered ({} bytes): {:?}",
            recovered.len(),
            &recovered[..recovered.len().min(16)]
        );
    }

    let perfect = original == recovered;
    if perfect {
        println!("loopback: PASS — bit-perfect reconstruction");
    } else {
        println!("loopback: FAIL — {error_bits} bit errors");
    }
    perfect
}
