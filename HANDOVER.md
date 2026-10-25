# Handover & Architecture Guide — `headphone-dsp`

This document provides the complete technical context for continuing work on **`headphone-dsp`** in a new AI session.

---

## 1. Project Overview

`headphone-dsp` is a standalone desktop **Crossfeed + 10-Band Graphic EQ** headphone processor for **Linux** and **Windows**, built with **Tauri v2 (Rust)** and an embedded **CamillaDSP 4.1.3** sidecar engine.

- **Repository:** `/home/alfredw/dev/headphone-dsp` (`https://github.com/alfredwidell/headphone-dsp.git`, branch `main`)
- **Related repository:** `/home/alfredw/dev/hifi` (the origin of the Neumann KU100 Mid/Side crossfeed algorithm in `core/hifi_core/crossfeed.py` and the dark purple UI styling)
- **Canonical long-term wiki:** `/home/alfredw/dev/me-wiki/projects/headphone-dsp.md`

---

## 2. Architecture & Key Files

```text
headphone-dsp/
├── HANDOVER.md                 # This handover document
├── README.md                   # User/developer build & usage docs
├── .repo-wiki.md               # Pointer to ~/dev/me-wiki/projects/headphone-dsp.md
├── ui/                         # Dependency-free vanilla JS + CSS frontend
│   ├── index.html              # Single-window studio layout & dialogs
│   ├── style.css               # Dark glassmorphism theme (#08090c, #b98af0 purple accent)
│   ├── app.js                  # Tauri invoke bridge, SVG EQ drag/wheel, zoom, browser mock fallback
│   ├── fonts/                  # Space Grotesk + Manrope variable woff2 fonts
│   └── vendor/                 # Bundled assets
└── src-tauri/                  # Rust / Tauri v2 backend
    ├── Cargo.toml              # Crate manifest (headphone_dsp_lib + headphone-dsp binary)
    ├── tauri.conf.json         # Tauri config (withGlobalTauri: true, externalBin: binaries/camilladsp)
    ├── binaries/
    │   ├── camilladsp-x86_64-unknown-linux-gnu      # CamillaDSP 4.1.3 (Linux amd64, Pulse + ALSA)
    │   └── camilladsp-x86_64-pc-windows-msvc.exe    # CamillaDSP 4.1.3 (Windows amd64, WASAPI)
    ├── data_ku100/             # 182 baked Neumann KU100 HRIR JSON files (0°–90° at 44.1 & 48 kHz)
    ├── testdata/
    │   └── crossfeed_ms_35_44100_3db.json           # Python reference fixture for 1e-9 parity test
    └── src/
        ├── main.rs             # Entry point calling headphone_dsp_lib::run()
        ├── lib.rs              # Tauri commands, state mutex, device/volume lifecycle
        ├── state.rs            # AppSettings, DspSettings, IoConfig, JSON persistence & profiles
        ├── crossfeed.rs        # Rust port of MS KU100 crossfeed FIR synthesis + preview curves
        ├── peq.rs              # RBJ biquad math, auto-preamp calculation, AutoEQ parser/exporter
        ├── devices.rs          # Linux PipeWire/Pulse virtual sink + ALSA & Windows WASAPI discovery
        └── camilla.rs          # CamillaDSP YAML/JSON config builder & WebSocket sidecar manager
```

---

## 3. Core Technical Details & Invariants

### 3.1 Neumann KU100 Mid/Side Crossfeed (`src-tauri/src/crossfeed.rs`)
- Exact port of `/home/alfredw/dev/hifi/core/hifi_core/crossfeed.py`.
- Embeds `src-tauri/data_ku100/{44100,48000}/angle_000.json..angle_090.json` at compile time via `include_dir!`.
- Computes the relative contralateral transfer function $h = \text{contra} / \text{ipsi}[0]$, then designs spectral-division Mid and Side filters ($2048$ taps, $32768$-point FFT, $512$-sample modeling delay, $64$-tap raised-cosine tail fade) with a 4th-order soft-knee boost cap (`crossfeed_mid_boost_db`, $0\text{--}18\text{ dB}$):
  - $\text{Mid} = \text{correction} \cdot (1 + h)$
  - $\text{Side} = \text{correction} \cdot (1 - h)$
- **Parity guarantee:** `crossfeed::tests::ms_filters_match_python_reference` verifies all 2048 Mid and Side taps match Python to within `< 1e-9`.

### 3.2 CamillaDSP Pipeline & Hot-Reload (`src-tauri/src/camilla.rs`)
- **Stage order:**
  1. `preamp_gain` (`type: Gain`, channels `[0, 1]`) — omitted when `|effective_preamp_db| <= 1e-3` or `master_bypass == true`.
  2. Literal Mid/Side Crossfeed (`crossfeed_to_mid_side` mixer at `-6.0206 dB` → `crossfeed_mid_filter` on ch 0 & `crossfeed_side_filter` on ch 1 → `crossfeed_from_mid_side` mixer) — omitted when `!crossfeed_enabled || master_bypass`.
     - **Important live hot-reload rule (`conv_filter`):** `crossfeed_mid_filter` and `crossfeed_side_filter` use inline `"type": "Values", "values": taps` (rather than `"type": "Raw", "filename": "..."`) so CamillaDSP's config-diff detects tap changes during `SetConfigJson` and hot-reloads the Mid/Side FIR filters immediately while audio is playing.
  3. 10-Band ISO Graphic EQ (`parametric_eq_band_0..9`, `type: Biquad` `Peaking` at `31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000 Hz`, `Q = 1.41`, `±6 dB` range).
     - **Important CamillaDSP 4.1.3 rule:** CamillaDSP rejects a single `Filter` pipeline step if `channels` contains duplicate indices, and we emit a single step `{"type": "Filter", "channels": [0, 1], "names": band_names}` applying all 10 bands to both channels.
     - **AutoEQ Import/Fitting (`src-tauri/src/peq.rs`):** 10-band AutoEQ `FixedBandEQ` presets map 1:1 on the fast path; arbitrary AutoEQ parametric presets (`LS`, `HS`, `PK`) and `GraphicEQ:` curves are fitted onto the 10 ISO bands via 4 rounds of regularized least squares.
- **Hot-reload vs Restart:**
  - When DSP parameters change (`crossfeed_*`, `eq_*`, `master_bypass`) while the IO route (`capture`, `playback`, `sample_rate`, `chunksize`, `exclusive`) is unchanged, `Core::hot_push` regenerates the FIR files/config and sends `{"SetConfigJson": "<json>"}` over CamillaDSP's local WebSocket without interrupting audio.
  - When the IO route changes, `Core::launch` restarts the CamillaDSP child process on a fresh free local TCP port.

### 3.3 Audio Routing & Volume Architecture (`src-tauri/src/devices.rs`, `src-tauri/src/lib.rs`)
- **Linux:**
  - On startup / device refresh, `ensure_linux_virtual_sink()` ensures the PipeWire/PulseAudio null sink `headphone_dsp_sink` (`description="Headphone_DSP_Virtual_Input"`) is loaded via `pactl`.
  - When `headphone_dsp_sink` is active, `list.capture` contains **only** `headphone_dsp_sink.monitor` (`type: Pulse`), and the UI automatically hides the Capture dropdown (`#capture-wrap`) so the user only selects their **Headphone output / DAC**.
  - **Automated Sample Rate, Resampling & Buffer (`DEFAULT_CHUNKSIZE = 512`):** Source sample rate (`44.1 kHz`, `48 kHz`, `88.2 kHz`, `96 kHz`, etc.) is detected automatically from the active source (`detect_source_sample_rate`: active sink-input streams / monitor source on Linux via `pactl`, or endpoint default config on Windows via `cpal`). Any higher source rate is mapped via `ku100_base_sample_rate()` to its integer-divisor KU100 base rate (`44.1 kHz` for `88.2/176.4/352.8 kHz`, `48 kHz` for `96/192/384 kHz`) and resampled by CamillaDSP (`capture_samplerate` + `resampler: {type: Synchronous}`), with a fixed `512`-sample low-latency buffer (`chunksize: 512`).
  - **Single-Volume-Control Guarantee:**
    - `maximize_linux_output_gains()` pins all hardware ALSA mixer controls (`Master`, `Headphone`, `PCM`, `Front`, `DAC` across cards `0..3`) and any selected downstream Pulse/PipeWire output sink to `100%` (`0.00 dB`, unmuted).
    - `Auto-preamp` is always enabled (`auto_preamp = true`, `preamp_db = 0.0`) and integrated directly into the Graphic EQ toolbar (`Auto-preamp −X.X dB · Peak +X.X dB`), applying `min(-raw_peak_db, 0.0)` (`0.0 dB` when no positive EQ boost is active).
    - **OS Volume mode (`hw_volume_bypass == false`, default):** The user controls volume via the normal OS volume slider/keys on `Headphone DSP Virtual Input` (`headphone_dsp_sink`). Its level is tracked in `settings.virtual_sink_volume` (`~/.config/headphone-dsp/settings.json`) and restored on app startup.
    - **HW VOL Bypass mode (`hw_volume_bypass == true`, toggled via the header `HW VOL` button):** Saves the previous OS volume, then locks `headphone_dsp_sink` to `100%` (`0.00 dB`) — and re-enforces `100%` during `get_live_meters` polling — for external DACs/amps with their own hardware volume pot. Toggling `HW VOL` off restores the saved OS volume.
- **Windows:**
  - Uses Option 1 — Zero-driver WASAPI Loopback: enumerates Windows output endpoints via `cpal` and exposes each as a capture option with `backend: "Wasapi"`, `is_loopback: true` (`loopback: true`, `format: "F32_LE"` in CamillaDSP) alongside standard WASAPI playback endpoints (`exclusive: bool` supported).

### 3.4 UI & Zoom (`ui/index.html`, `ui/style.css`, `ui/app.js`)
- **3-Group Top Header Bar (`.topbar`):**
  1. `.brand` (`HEADPHONE DSP` title + subtitle, separated by right border)
  2. `.meter-section` (`● Running <rate>` + expanded stereo `L/R` output meter with direct red visual clip indication; click meter to clear)
  3. `.engine-controls` (`HW VOL` with `#hwvol-dialog` safety confirmation modal before locking to `100% / 0 dB`, `A / B` full-workspace bypass, and `Start/Stop DSP`)
- **Unified Processor Rack Card (`main.workspace.card`):**
  - **50% / 50% Top Rack Header (`.rack-header`):** Left half holds `OUTPUT / DAC` (`.routing-pod`); right half holds `PROFILE` (`#profiles`, `Save as…`, `⌫`, `AutoEQ ↗` with clickable `autoeq.app` preset search link via `open_external_url`).
  - **Left 1/3 Pane (`SPATIAL PROCESSING` / `#crossfeed-card`):** Speaker angle (`0°–90°`), Mid flatness (`0–18 dB`), Virtual Listening Stage diagram, and real-data KU100 Mid/Side/ILD response graph (`get_crossfeed_preview` via `spawn_blocking`, `-24..+12 dB`) with 3 color-coded explanatory rows below.
  - **Right 2/3 Pane (`FREQUENCY SHAPING` / `#eq-card`):** Composite EQ graph (`0` to `−18 dB`, remaining visible in dimmed grey when EQ is switched off) above a 10-band studio vertical fader bank (`±6 dB` range, `184px` fader height).
  - **Full-Workspace Bypass Overlay (`#bypass-overlay`):** Covers and dims both Spatial Processing and Frequency Shaping panes when `A / B` bypass is active.
- **UI Zoom:** Supports **`Ctrl +` / `Ctrl −` / `Ctrl 0`** and **`Ctrl + Mousewheel`** (`70%`–`200%`), persisted in `localStorage` (`headphone-dsp.ui-zoom.v1`) and applied via Tauri's native `WebviewWindow::set_zoom` (`set_webview_zoom` command).

---

## 4. Runtime Paths & Commands

- **User config & state directory:**
  - Linux: `~/.config/headphone-dsp/` (`settings.json`, `profiles/*.json`, `runtime/active.yml`, `runtime/camilladsp.log`, `runtime/generated_fir/{mid,side}_filter.txt`)
  - Windows: `%APPDATA%\headphone-dsp\`
- **Installed convenience symlinks on this Linux workstation (`x13`):**
  - Binary: `~/.local/bin/headphone-dsp` → `/home/alfredw/dev/headphone-dsp/src-tauri/target/release/headphone-dsp`
  - Sidecar: `~/.local/bin/camilladsp` → `/home/alfredw/dev/headphone-dsp/src-tauri/binaries/camilladsp-x86_64-unknown-linux-gnu`
  - Desktop launcher: `~/.local/share/applications/headphone-dsp.desktop`
- **Build & test commands:**
  ```bash
  export PATH="$HOME/.cargo/bin:$PATH"
  cargo test --manifest-path src-tauri/Cargo.toml
  cargo build --release --manifest-path src-tauri/Cargo.toml
  ```

---

## 5. Next Steps / Open Items for Future Sessions

1. **Windows Testing & Packaging:**
   - Test compilation and live WASAPI loopback capture + WASAPI shared/exclusive playback on a Windows 10/11 machine (`cargo build --release --manifest-path src-tauri/Cargo.toml` or `cargo tauri build` for `.msi` / NSIS installer).
   - Verify whether Windows WASAPI endpoint volume should also be pinned to `100%` when `HW VOL` (`hw_volume_bypass`) is enabled on Windows.
2. **Linux Packaging / Autostart (Optional):**
   - Optionally package as `.deb` / `.AppImage` via `cargo tauri build`, or add a system-tray minimize / autostart option so the DSP engine can run in the background at login.
3. **Additional Features (Optional):**
   - Per-channel L/R balance trim or L/R channel swap / polarity invert in the Preamp card.
   - Built-in searchable AutoEQ headphone preset library.
