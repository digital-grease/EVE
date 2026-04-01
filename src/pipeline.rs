/// Async pipeline tying framing → codec → VoIP together.
use crate::codec::{MfskDecoder, MfskEncoder};
use crate::config::Config;
use crate::framing;
use crate::framing::depacketizer::Depacketizer;
use crate::framing::packetizer::{serialize_frame, serialize_nak_frame, split_frames, Packetizer};
use crate::transport::udp::UdpTransport;
use crate::voip::rtp::{pcm_to_ulaw, ulaw_to_pcm, DejitterBuffer, RtpSession};
use crate::voip::sip::SipAgent;
use crate::wav;
use std::collections::HashMap;
use std::io::BufWriter;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, timeout, Duration, MissedTickBehavior};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Send PCM samples as a paced RTP burst.
///
/// Converts samples to μ-law, wraps in RTP headers, and paces output at
/// one 160-sample packet per 20 ms (50 pps, matching G.711 at 8 kHz).
async fn send_rtp_burst(
    samples: &[i16],
    transport: &UdpTransport,
    dest: SocketAddr,
    rtp_sess: &mut RtpSession,
    verbose: bool,
) {
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut sent = 0usize;
    for chunk in samples.chunks(160) {
        tick.tick().await;
        let ulaw = pcm_to_ulaw(chunk);
        let packet = rtp_sess.build_packet(&ulaw);
        if let Err(e) = transport.send_to(&packet, dest).await {
            warn!("RTP send error: {e}");
        }
        sent += 1;
        if verbose && sent.is_multiple_of(50) {
            info!("sent {sent} RTP packets");
        }
    }
}

/// Collect RTP μ-law payloads until no packet arrives within `idle_timeout`.
///
/// Strips the 12-byte RTP fixed header from each datagram.  Returns as soon
/// as either `idle_timeout` elapses with no new packet or `max_duration`
/// is exhausted.  Returns an empty vec if nothing arrives at all.
async fn collect_rtp_burst(
    transport: &UdpTransport,
    idle_timeout: Duration,
    max_duration: Duration,
) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + max_duration;
    let mut all_ulaw: Vec<u8> = Vec::new();
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        let wait = idle_timeout.min(remaining);
        match timeout(wait, transport.recv_from()).await {
            Ok(Ok((data, _))) if data.len() > 12 => {
                all_ulaw.extend_from_slice(&data[12..]);
            }
            _ => break,
        }
    }
    all_ulaw
}

// ---------------------------------------------------------------------------
// Sender pipeline
// ---------------------------------------------------------------------------

/// Run the sender pipeline: file → frames → mFSK PCM → RTP → UDP.
///
/// If `config.arq.retries > 0` the sender enters a post-transfer ARQ loop:
/// it parks on its RTP socket and waits for a NAK mFSK burst from the receiver.
/// On receipt it decodes the missing sequence list and retransmits those frames.
pub async fn run_sender(file: PathBuf, dest: SocketAddr, config: Config) {
    // ---- SIP signalling ----
    let mut sip = SipAgent::new(config.voip.clone());
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
    // Returns the encoded PCM stream AND a per-seq serialised-frame map used
    // for ARQ retransmissions.
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

        // Serialise ALL frames into one contiguous byte buffer; also keep a
        // per-seq map for selective ARQ retransmission.
        let mut frame_bytes: HashMap<u32, Vec<u8>> = HashMap::new();
        let mut all_bytes: Vec<u8> = Vec::new();
        for frame in &frames {
            let serialized = serialize_frame(frame);
            frame_bytes.insert(frame.seq, serialized.clone());
            all_bytes.extend_from_slice(&serialized);
        }

        let enc = MfskEncoder::new(cfg_clone.codec.clone())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;

        let samples = enc.encode(&all_bytes);
        Ok::<_, std::io::Error>((samples, frame_bytes))
    })
    .await
    .expect("blocking task panicked");

    let (samples, frame_bytes) = match encode_result {
        Ok(v) => v,
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
    let transport = match UdpTransport::bind(rtp_local).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("RTP bind error: {e}");
            return;
        }
    };

    let mut rtp_sess = RtpSession::new(config.voip.clone());
    send_rtp_burst(&samples, &transport, rtp_dest, &mut rtp_sess, config.verbose).await;
    info!("initial transfer complete");

    // ---- ARQ retransmission loop (before BYE so the channel stays open) ----
    if config.arq.retries > 0 && !frame_bytes.is_empty() {
        let idle = Duration::from_millis(500);
        let max_wait = Duration::from_millis(config.arq.timeout_ms);

        for retry in 1..=config.arq.retries {
            // Wait for NAK mFSK burst from receiver.
            let nak_ulaw = collect_rtp_burst(&transport, idle, max_wait).await;
            if nak_ulaw.is_empty() {
                info!("ARQ: no NAK received, transfer complete");
                break;
            }

            // Decode NAK → missing seq list.
            let cfg_c = config.codec.clone();
            let missing_result =
                tokio::task::spawn_blocking(move || -> Result<Vec<u32>, std::io::Error> {
                    let pcm = ulaw_to_pcm(&nak_ulaw);
                    let dec = MfskDecoder::new(cfg_c).map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
                    })?;
                    let bytes = dec.decode(&pcm).map_err(std::io::Error::other)?;
                    let frames = split_frames(&bytes).map_err(std::io::Error::other)?;
                    for f in &frames {
                        if f.flags & framing::flags::NAK != 0 {
                            let seqs = f
                                .payload
                                .chunks_exact(4)
                                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
                                .collect();
                            return Ok(seqs);
                        }
                    }
                    Ok(vec![])
                })
                .await
                .expect("blocking task panicked");

            let missing = match missing_result {
                Ok(m) => m,
                Err(e) => {
                    warn!("ARQ: failed to decode NAK on retry {retry}: {e}");
                    continue;
                }
            };

            if missing.is_empty() {
                info!("ARQ: empty NAK on retry {retry}, transfer complete");
                break;
            }

            info!(
                "ARQ retry {retry}/{}: retransmitting {} frames",
                config.arq.retries,
                missing.len()
            );

            // Encode only the missing frames.
            let retry_bytes: Vec<u8> = missing
                .iter()
                .filter_map(|&seq| frame_bytes.get(&seq))
                .flat_map(|b| b.iter().copied())
                .collect();

            let cfg_c = config.codec.clone();
            let retry_samples = tokio::task::spawn_blocking(move || {
                let enc = MfskEncoder::new(cfg_c).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
                })?;
                Ok::<Vec<i16>, std::io::Error>(enc.encode(&retry_bytes))
            })
            .await
            .expect("blocking task panicked");

            match retry_samples {
                Ok(s) => {
                    send_rtp_burst(&s, &transport, rtp_dest, &mut rtp_sess, config.verbose).await;
                }
                Err(e) => warn!("ARQ: encode error on retry {retry}: {e}"),
            }
        }
    }

    // ---- BYE (after ARQ loop so the channel stays open for retransmissions) ----
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
///
/// If frames are missing after the initial decode and `config.arq.retries > 0`,
/// the receiver enters an ARQ loop: it encodes the missing sequence list as a
/// NAK mFSK burst and sends it back to the caller, then waits for retransmitted
/// frames.  The loop repeats until the file is complete or retries are exhausted.
async fn receive_one(output_dir: &Path, config: &Config) {
    let voip = config.voip.clone();
    let codec = config.codec.clone();

    // ---- SIP signalling ----
    let mut sip = SipAgent::new(voip.clone());
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
    let transport = match UdpTransport::bind(rtp_local).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("RTP bind error: {e} — call accepted but cannot receive media");
            return;
        }
    };

    // Clone the transport handle for ARQ use after the RTP receive task exits.
    // Both handles share the same underlying socket (Arc<UdpSocket>).
    let transport_for_arq = transport.clone();

    // BYE waiter sends a oneshot when the call ends.
    // Share the SIP transport from accept() so we don't rebind the port.
    let (bye_tx, bye_rx) = oneshot::channel::<()>();
    let mut sip_bye = SipAgent::new(voip.clone());
    sip_bye.share_transport_from(&sip);
    let call_id_for_bye = call_id.clone();
    tokio::spawn(async move {
        let _ = sip_bye.wait_for_bye(&call_id_for_bye).await;
        let _ = bye_tx.send(());
    });

    // μ-law payload channel: RTP loop → collector.
    let (ulaw_tx, mut ulaw_rx) = mpsc::channel::<Vec<u8>>(256);

    // RTP receive loop: exits when BYE fires, flushes dejitter buffer first.
    // `transport` is moved into this task; `transport_for_arq` remains in scope.
    let jitter_ms = voip.jitter_ms;
    tokio::spawn(async move {
        let mut jb = DejitterBuffer::new(jitter_ms);
        let mut bye_rx = bye_rx;

        loop {
            tokio::select! {
                _ = &mut bye_rx => {
                    for p in jb.flush() {
                        if ulaw_tx.send(p).await.is_err() { break; }
                    }
                    break;
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
        // ulaw_tx dropped here → ulaw_rx.recv() returns None → collector exits
    });

    // Collect all μ-law payloads (terminates when ulaw_tx is dropped above).
    let mut all_ulaw: Vec<u8> = Vec::new();
    while let Some(payload) = ulaw_rx.recv().await {
        all_ulaw.extend_from_slice(&payload);
    }
    info!("RTP stream ended, {} μ-law bytes collected", all_ulaw.len());

    // ---- Decode (blocking) ----
    // Returns (output_path, depacketizer) so the ARQ loop can feed more frames
    // to the same depacketizer instance if some were missing on the first pass.
    let output_dir_clone = output_dir.to_path_buf();
    let verbose = config.verbose;
    let decode_result = tokio::task::spawn_blocking(move || {
        let pcm = ulaw_to_pcm(&all_ulaw);
        let dec = MfskDecoder::new(codec)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        let all_bytes = if verbose {
            let (bytes, snr_ratios) = dec
                .decode_verbose(&pcm)
                .map_err(std::io::Error::other)?;
            if !snr_ratios.is_empty() {
                let min_snr = snr_ratios.iter().cloned().fold(f64::INFINITY, f64::min);
                let avg_snr = snr_ratios.iter().sum::<f64>() / snr_ratios.len() as f64;
                let low_confidence = snr_ratios.iter().filter(|&&r| r < 3.0).count();
                eprintln!(
                    "mFSK decode: {} symbols, SNR min={:.1} avg={:.1}, low-confidence={}/{}",
                    snr_ratios.len(),
                    min_snr,
                    avg_snr,
                    low_confidence,
                    snr_ratios.len(),
                );
            }
            bytes
        } else {
            dec.decode(&pcm).map_err(std::io::Error::other)?
        };
        let frames = split_frames(&all_bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;

        if frames.is_empty() {
            return Err(std::io::Error::other("no valid frames decoded"));
        }

        let mut depack = Depacketizer::new(output_dir_clone);
        let mut output_path = None;
        for frame in frames {
            match depack.push(frame) {
                Ok(Some(path)) => {
                    output_path = Some(path);
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e.to_string(),
                    ));
                }
            }
        }
        Ok((output_path, depack))
    })
    .await
    .expect("blocking task panicked");

    let (output_path, mut depack) = match decode_result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("decode error: {e}");
            return;
        }
    };

    if let Some(ref path) = output_path {
        info!("file written to {path:?}");
        return;
    }

    // ---- ARQ response loop ----
    if config.arq.retries == 0 {
        warn!("incomplete transfer — missing frames");
        return;
    }

    let mut rtp_sess = RtpSession::new(voip.clone());
    let idle = Duration::from_millis(500);
    let max_wait = Duration::from_millis(config.arq.timeout_ms);

    for retry in 1..=config.arq.retries {
        let missing = depack.missing_seqs();
        if missing.is_empty() {
            break;
        }

        info!(
            "ARQ pass {retry}/{}: {} frames missing, sending NAK",
            config.arq.retries,
            missing.len()
        );

        // Encode NAK frame → mFSK → send to sender.
        let nak_bytes = serialize_nak_frame(&missing);
        let cfg_c = config.codec.clone();
        let nak_samples = match tokio::task::spawn_blocking(move || {
            let enc = MfskEncoder::new(cfg_c).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
            })?;
            Ok::<Vec<i16>, std::io::Error>(enc.encode(&nak_bytes))
        })
        .await
        .expect("blocking task panicked")
        {
            Ok(s) => s,
            Err(e) => {
                warn!("ARQ: NAK encode error: {e}");
                break;
            }
        };

        send_rtp_burst(
            &nak_samples,
            &transport_for_arq,
            caller_rtp_addr,
            &mut rtp_sess,
            false,
        )
        .await;

        // Wait for retransmission from sender.
        let retry_ulaw = collect_rtp_burst(&transport_for_arq, idle, max_wait).await;
        if retry_ulaw.is_empty() {
            warn!("ARQ: no retransmission received on pass {retry}");
            break;
        }

        // Decode retransmission and push to the live depacketizer.
        let codec_c = config.codec.clone();
        let retry_frames = match tokio::task::spawn_blocking(move || {
            let pcm = ulaw_to_pcm(&retry_ulaw);
            let dec = MfskDecoder::new(codec_c).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
            })?;
            let bytes = dec.decode(&pcm).map_err(std::io::Error::other)?;
            split_frames(&bytes).map_err(std::io::Error::other)
        })
        .await
        .expect("blocking task panicked")
        {
            Ok(frames) => frames,
            Err(e) => {
                warn!("ARQ: decode error on retry {retry}: {e}");
                continue;
            }
        };

        for frame in retry_frames {
            match depack.push(frame) {
                Ok(Some(path)) => {
                    info!("file written to {path:?}");
                    return;
                }
                Ok(None) => {}
                Err(e) => warn!("ARQ: depacketizer error: {e}"),
            }
        }
    }

    let remaining = depack.missing_seqs();
    if remaining.is_empty() {
        info!("ARQ: all frames received");
    } else {
        warn!(
            "incomplete transfer after {} ARQ retries — {} frames still missing",
            config.arq.retries,
            remaining.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Loopback mode
// ---------------------------------------------------------------------------

/// Run a local loopback test: encode then decode without network.
///
/// `loss_rate` (0.0–1.0) causes that fraction of encoded samples to be randomly
/// zeroed before decoding, simulating channel loss for ARQ testing.
/// Prints bit-error-rate (BER) and whether reconstruction is bit-perfect.
/// Returns `true` on bit-perfect reconstruction.
pub fn run_loopback(file: PathBuf, config: Config, loss_rate: f64) -> bool {
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

    // Simulate channel loss by randomly zeroing samples before decoding.
    let samples = if loss_rate > 0.0 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut lossy = samples;
        let mut h = DefaultHasher::new();
        for (i, s) in lossy.iter_mut().enumerate() {
            i.hash(&mut h);
            let v = h.finish();
            let frac = (v & 0xFFFF) as f64 / 65535.0;
            if frac < loss_rate {
                *s = 0;
            }
        }
        lossy
    } else {
        samples
    };

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
