# Headphone DSP

Standalone desktop **Crossfeed + 10-Band Graphic EQ** headphone processor for **Linux** and **Windows**, built with **Tauri v2 (Rust)** and an embedded **CamillaDSP 4.1.3** engine.

## Features

- **Neumann KU100 Mid/Side Binaural Crossfeed**:
  - Uses measured horizontal-plane Neumann KU100 dummy-head HRIRs (`0°–90°` at `44.1 kHz` and `48 kHz`). Higher source sample rates (`88.2`, `96`, `176.4`, `192 kHz`, etc.) are automatically resampled by CamillaDSP to the integer-divisor base rate (`44.1 kHz` or `48 kHz`) so measured KU100 HRIR filters are always used.
  - Applies spectral-division **Mid flatness** correction (`0–18 dB` soft-knee limiter + 512-sample modeling delay) so centered mono content (vocals, bass, snare) stays flat and forward while the stereo Side channel carries the exact measured KU100 interaural level, frequency, and phase/ITD cues (`contra / ipsi == h`).
  - Literal Mid/Side CamillaDSP pipeline (`crossfeed_to_mid_side` mixer → 2048-tap `Conv` filters on Mid and Side → `crossfeed_from_mid_side` mixer).
- **10-Band AutoEQ Graphic Equalizer (`Q = 1.41`)**:
  - ISO octave centers (`31, 62, 125, 250, 500, 1k, 2k, 4k, 8k, 16k Hz`) with constant `Q = 1.41` peaking biquads, matching AutoEQ's 10-band Fixed-Band EQ.
  - Interactive frequency-response graph (20 Hz – 20 kHz) with vertical-drag band handles plus a 10-channel studio vertical fader bank (drag, mousewheel, keyboard, numeric input, and double-click to reset).
  - **Composite Automatic Anti-Clip Preamp**: Evaluates the combined frequency response of the active KU100 Mid/Side Crossfeed FIR filters and 10-band Graphic EQ (`composite_chain_headroom_db`) bin-by-bin across frequency and attenuates by the composite peak (`min(-raw_peak_db, 0)`) so non-overlapping boosts are not double-penalized while the output never exceeds `0 dB`.
  - **Headphone Profiles & AutoEQ Import/Export**: Save/load named headphone profiles and import/export AutoEQ presets (supports 10-band Fixed-Band EQ directly, and automatically fits arbitrary AutoEQ Parametric / GraphicEQ curves onto the 10 bands).
- **Zero-Driver Loopback Capture**:
  - **Windows (Option 1 — Zero-driver WASAPI Loopback)**: Captures system audio from a Windows playback endpoint via CamillaDSP's native `Wasapi` `loopback: true` capture mode and outputs to your headphone DAC via `Wasapi` (shared or exclusive mode).
  - **Linux (PipeWire / PulseAudio + ALSA)**: Automatically creates a `"HeadphoneDSP"` sink (`headphone_dsp_sink`) on startup, captures from `headphone_dsp_sink.monitor`, and outputs directly to your hardware DAC via ALSA or PipeWire/PulseAudio.
- **Live Signal Meters & Level-Matched A/B Bypass**:
  - Real-time input/output stereo peak & RMS meters, DSP processing load, and clipped-sample counter over CamillaDSP's local WebSocket control port.
  - **`A / B` Bypass** bypasses the Crossfeed and Graphic EQ filters while keeping the active `preamp_gain` offset so unprocessed vs. processed comparisons stay level-matched without volume jumps.

## Repository Layout

```text
headphone-dsp/
├── ui/                         # Frontend Web UI (vanilla JS + CSS, dark purple studio theme)
├── src-tauri/
│   ├── binaries/               # Bundled CamillaDSP 4.1.3 binaries (Linux x86_64 & Windows x86_64)
│   ├── data_ku100/             # Baked Neumann KU100 HRIR tables (0°–90°, 44.1 kHz & 48 kHz)
│   └── src/
│       ├── crossfeed.rs        # Rust port of MS KU100 crossfeed FIR synthesis
│       ├── peq.rs              # RBJ biquad frequency-response math & AutoEQ parser
│       ├── devices.rs          # Linux PipeWire/ALSA & Windows WASAPI loopback discovery
│       ├── camilla.rs          # CamillaDSP YAML/JSON builder & WebSocket sidecar manager
│       ├── state.rs            # Persistent settings & headphone profile store
│       └── lib.rs              # Tauri v2 command handlers
```

## Development & Building

### Prerequisites (Ubuntu / Debian Linux)

```bash
sudo apt-get install -y \
  build-essential pkg-config libssl-dev \
  libgtk-3-dev libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev \
  libasound2-dev libpulse-dev patchelf
```

### Run Unit Tests

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```

### Build & Run (Linux)

```bash
cargo build --release --manifest-path src-tauri/Cargo.toml
./src-tauri/target/release/headphone-dsp
```

### Build on Windows (x86_64)

1. Install [Rust (`rustup`)](https://rustup.rs/) with the MSVC toolchain.
2. Run:
   ```powershell
   cargo build --release --manifest-path src-tauri/Cargo.toml
   ```
   The bundled `src-tauri/binaries/camilladsp-x86_64-pc-windows-msvc.exe` is used automatically.
