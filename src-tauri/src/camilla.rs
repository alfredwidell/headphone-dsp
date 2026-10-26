//! CamillaDSP pipeline construction and process/websocket control.
//!
//! The config builder is a pure function of (IO config, DSP settings,
//! runtime dir). Port of `hifi_core/camilla_builders/headphone.py` plus the
//! preamp and parametric-EQ stages requested for this app.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::devices::{canonical_backend, BACKEND_PULSE, BACKEND_WASAPI};
use crate::crossfeed::{
    build_ms_mid_filter_taps, build_ms_side_filter_taps, coerce_crossfeed_angle_deg,
    coerce_crossfeed_mid_boost_db, write_text_fir_file, MS_FILTER_TAP_COUNT,
};
use crate::peq::{compute_peq_response, EqBand};
use crate::state::{DspSettings, IoConfig};

const GAIN_CROSSFEED_HALF_DB: f64 = -6.0206;
const PREAMP_EPSILON_DB: f64 = 1e-3;

fn conv_filter(taps: &[f64]) -> Value {
    json!({
        "type": "Conv",
        "parameters": {
            "type": "Values",
            "values": taps
        }
    })
}

fn mixer_source(channel: usize, gain: f64, inverted: bool) -> Value {
    json!({"channel": channel, "gain": gain, "inverted": inverted, "mute": false})
}

fn mixer_dest(dest: usize, sources: Vec<Value>) -> Value {
    json!({"dest": dest, "mute": false, "sources": sources})
}

/// CamillaDSP `devices.capture` block. Capture is always stereo.
///
/// Sample-format names are CamillaDSP 4.x's (`S16_LE`, `S32_LE`, `F32_LE`).
/// ALSA capture uses S16_LE: snd-aloop loopback captures only negotiate
/// S16_LE in practice (see the hifi project's headphone builder). The Pulse
/// block takes no format key.
fn capture_device(io: &IoConfig) -> Result<Value, String> {
    let device = &io.capture;
    let backend = canonical_backend(&device.backend)
        .ok_or_else(|| format!("unsupported capture backend '{}'", device.backend))?;
    match backend {
        BACKEND_WASAPI => Ok(json!({
            "type": "Wasapi",
            "channels": 2,
            "device": device.id,
            "format": "F32_LE",
            "exclusive": false,
            "loopback": device.is_loopback
        })),
        BACKEND_PULSE => Ok(json!({
            "type": "Pulse",
            "channels": 2,
            "device": device.id
        })),
        _ => Ok(json!({
            "type": "Alsa",
            "channels": 2,
            "device": device.id,
            "format": "S16_LE"
        })),
    }
}

/// CamillaDSP `devices.playback` block. Playback is always stereo.
fn playback_device(io: &IoConfig) -> Result<Value, String> {
    let device = &io.playback;
    let backend = canonical_backend(&device.backend)
        .ok_or_else(|| format!("unsupported playback backend '{}'", device.backend))?;
    match backend {
        BACKEND_WASAPI => Ok(json!({
            "type": "Wasapi",
            "channels": 2,
            "device": device.id,
            "format": "F32_LE",
            "exclusive": io.exclusive
        })),
        BACKEND_PULSE => Ok(json!({
            "type": "Pulse",
            "channels": 2,
            "device": device.id
        })),
        _ => Ok(json!({
            "type": "Alsa",
            "channels": 2,
            "device": device.id,
            "format": "S32_LE"
        })),
    }
}

/// Build the complete CamillaDSP configuration (as JSON, which is also
/// valid YAML). Writes crossfeed FIR files under `runtime_dir/generated_fir`.
///
/// Pipeline order: preamp -> Mid/Side crossfeed -> parametric EQ. With
/// `master_bypass` set, `preamp_gain` is still applied so A/B comparisons
/// stay level-matched against the processed chain, while crossfeed and EQ
/// stages are omitted.
pub fn build_config(io: &IoConfig, dsp: &DspSettings, runtime_dir: &Path) -> Result<Value, String> {
    if io.sample_rate == 0 {
        return Err("sample rate must be positive".to_string());
    }
    let dsp = dsp.sanitized();
    // The DSP runs at the KU100 base rate (44.1 or 48 kHz). Higher source
    // rates are resampled down by CamillaDSP at capture.
    let source_fs = io.sample_rate;
    let fs = crate::crossfeed::ku100_base_sample_rate(source_fs);

    let mut filters = Map::new();
    let mut mixers = Map::new();
    let mut pipeline: Vec<Value> = Vec::new();

    // With EQ switched off the bands are left out of the pipeline entirely.
    let enabled_bands: Vec<EqBand> = if dsp.eq_enabled {
        dsp.eq_bands
            .iter()
            .filter(|b| b.enabled)
            .map(|b| b.sanitized())
            .collect()
    } else {
        Vec::new()
    };

    let crossfeed_taps = if dsp.crossfeed_enabled {
        let angle = coerce_crossfeed_angle_deg(dsp.crossfeed_angle_deg);
        let boost = coerce_crossfeed_mid_boost_db(dsp.crossfeed_mid_boost_db);
        let mid = build_ms_mid_filter_taps(angle, fs, MS_FILTER_TAP_COUNT, boost);
        let side = build_ms_side_filter_taps(angle, fs, MS_FILTER_TAP_COUNT, boost);
        Some((mid, side))
    } else {
        None
    };

    let auto_headroom_db = if dsp.auto_preamp {
        if let Some((ref mid, ref side)) = crossfeed_taps {
            crate::crossfeed::composite_chain_headroom_db(
                Some((mid.as_slice(), side.as_slice())),
                &enabled_bands,
                fs,
            )
        } else {
            compute_peq_response(&enabled_bands, 0.0, true, fs, dsp.eq_enabled)
                .effective_preamp_db
        }
    } else {
        0.0
    };
    let effective_preamp = auto_headroom_db + dsp.preamp_db;
    if effective_preamp.abs() > PREAMP_EPSILON_DB {
        filters.insert(
            "preamp_gain".to_string(),
            json!({
                "type": "Gain",
                "parameters": {"gain": effective_preamp, "inverted": false, "mute": false}
            }),
        );
        pipeline.push(json!({"bypassed": false, "type": "Filter", "channels": [0, 1], "names": ["preamp_gain"]}));
    }

    if !dsp.master_bypass {
        if let Some((mid, side)) = crossfeed_taps {
            let fir_dir = runtime_dir.join("generated_fir");
            let mid_path = fir_dir.join("mid_filter.txt");
            let side_path = fir_dir.join("side_filter.txt");
            write_text_fir_file(&mid_path, &mid)
                .map_err(|e| format!("writing {}: {e}", mid_path.display()))?;
            write_text_fir_file(&side_path, &side)
                .map_err(|e| format!("writing {}: {e}", side_path.display()))?;

            filters.insert("crossfeed_mid_filter".to_string(), conv_filter(&mid));
            filters.insert("crossfeed_side_filter".to_string(), conv_filter(&side));

            // Mid = (L+R)/2, Side = (L-R)/2
            mixers.insert(
                "crossfeed_to_mid_side".to_string(),
                json!({
                    "channels": {"in": 2, "out": 2},
                    "mapping": [
                        mixer_dest(0, vec![
                            mixer_source(0, GAIN_CROSSFEED_HALF_DB, false),
                            mixer_source(1, GAIN_CROSSFEED_HALF_DB, false),
                        ]),
                        mixer_dest(1, vec![
                            mixer_source(0, GAIN_CROSSFEED_HALF_DB, false),
                            mixer_source(1, GAIN_CROSSFEED_HALF_DB, true),
                        ]),
                    ]
                }),
            );
            // L = Mid + Side, R = Mid - Side
            mixers.insert(
                "crossfeed_from_mid_side".to_string(),
                json!({
                    "channels": {"in": 2, "out": 2},
                    "mapping": [
                        mixer_dest(0, vec![mixer_source(0, 0.0, false), mixer_source(1, 0.0, false)]),
                        mixer_dest(1, vec![mixer_source(0, 0.0, false), mixer_source(1, 0.0, true)]),
                    ]
                }),
            );

            pipeline.push(json!({"bypassed": false, "type": "Mixer", "name": "crossfeed_to_mid_side"}));
            pipeline.push(json!({"bypassed": false, "type": "Filter", "channels": [0], "names": ["crossfeed_mid_filter"]}));
            pipeline.push(json!({"bypassed": false, "type": "Filter", "channels": [1], "names": ["crossfeed_side_filter"]}));
            pipeline.push(json!({"bypassed": false, "type": "Mixer", "name": "crossfeed_from_mid_side"}));
        }

        let mut band_names: Vec<String> = Vec::with_capacity(enabled_bands.len());
        for (index, band) in enabled_bands.iter().enumerate() {
            let name = format!("parametric_eq_band_{index}");
            let mut parameters = Map::new();
            parameters.insert("type".to_string(), json!(band.camilla_type()));
            parameters.insert("freq".to_string(), json!(band.freq_hz));
            if band.uses_gain() {
                parameters.insert("gain".to_string(), json!(band.gain_db));
            }
            parameters.insert("q".to_string(), json!(band.q));
            filters.insert(
                name.clone(),
                json!({"type": "Biquad", "parameters": Value::Object(parameters)}),
            );
            band_names.push(name);
        }
        if !band_names.is_empty() {
            pipeline.push(json!({"bypassed": false, "type": "Filter", "channels": [0, 1], "names": band_names}));
        }
    }

    // Hardware playback (ALSA/WASAPI) runs CamillaDSP's rate controller, which
    // needs an async resampler on capture to track the independent clocks.
    // PulseAudio/PipeWire playback is driven by the server, so no adjustment.
    let playback_backend = canonical_backend(&io.playback.backend).unwrap_or_default();
    let rate_adjust = playback_backend != BACKEND_PULSE;

    let mut devices = json!({
        "samplerate": fs,
        "chunksize": io.chunk_size,
        "enable_rate_adjust": rate_adjust,
        "silence_threshold": -160,
        // Never pause playback on digital silence; pause/resume causes DAC XRUNs.
        "silence_timeout": 0,
        "capture": capture_device(io)?,
        "playback": playback_device(io)?
    });
    if rate_adjust {
        if let Some(map) = devices.as_object_mut() {
            map.insert("target_level".to_string(), json!(io.chunk_size * 2));
            map.insert("adjust_period".to_string(), json!(3));
            map.insert("capture_samplerate".to_string(), json!(source_fs));
            map.insert(
                "resampler".to_string(),
                json!({"type": "AsyncSinc", "profile": "Accurate"}),
            );
        }
    } else if source_fs != fs {
        // Integer ratios use CamillaDSP's synchronous resampler; anything
        // else (including upsampling from below the base rate) is asynchronous.
        let resampler = if source_fs > fs && source_fs % fs == 0 {
            json!({"type": "Synchronous"})
        } else {
            json!({"type": "AsyncSinc", "profile": "Accurate"})
        };
        if let Some(map) = devices.as_object_mut() {
            map.insert("capture_samplerate".to_string(), json!(source_fs));
            map.insert("resampler".to_string(), resampler);
        }
    }

    Ok(json!({
        "title": "Headphone DSP",
        "devices": devices,
        "filters": Value::Object(filters),
        "mixers": Value::Object(mixers),
        "pipeline": pipeline
    }))
}

/// Serialize a config to YAML at `path` (the file CamillaDSP is started with).
pub fn write_config_yaml(path: &Path, config: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let yaml = serde_yaml::to_string(config).map_err(|e| format!("serializing config: {e}"))?;
    std::fs::write(path, yaml).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Locate the CamillaDSP binary: next to the app executable (bundled
/// sidecar, plain name), or the target-suffixed sidecar in `binaries/`
/// (development runs).
pub fn sidecar_path() -> Result<PathBuf, String> {
    let triple = env!("TARGET_TRIPLE");
    let (plain, suffixed) = if cfg!(windows) {
        ("camilladsp.exe".to_string(), format!("camilladsp-{triple}.exe"))
    } else {
        ("camilladsp".to_string(), format!("camilladsp-{triple}"))
    };

    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(&plain));
            candidates.push(dir.join(&suffixed));
        }
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries").join(&suffixed));

    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| {
            let tried: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
            format!("CamillaDSP binary not found; tried: {}", tried.join(", "))
        })
}

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Level value reported for a silent or unavailable channel.
pub const SILENT_DB: f64 = -120.0;

/// Live pipeline status, shaped for the UI. Levels are per-channel `[L, R]`
/// dBFS values.
#[derive(Debug, Clone, Serialize)]
pub struct LiveMeters {
    pub running: bool,
    pub state: String,
    /// Sample rate the pipeline runs at (or the backend's current choice when stopped).
    pub sample_rate: u32,
    /// Chunk size the pipeline runs with.
    pub chunksize: usize,
    pub capture_peak: Vec<f64>,
    pub playback_peak: Vec<f64>,
    pub capture_rms: Vec<f64>,
    pub playback_rms: Vec<f64>,
    pub load_percent: f64,
    pub clipped_samples: u64,
    pub error: Option<String>,
}

impl LiveMeters {
    fn silent() -> Vec<f64> {
        vec![SILENT_DB, SILENT_DB]
    }

    /// Meters for a pipeline that is not running.
    pub fn stopped(error: Option<String>) -> Self {
        Self {
            running: false,
            state: "Stopped".to_string(),
            sample_rate: crate::state::DEFAULT_SAMPLE_RATE,
            chunksize: crate::state::DEFAULT_CHUNKSIZE,
            capture_peak: Self::silent(),
            playback_peak: Self::silent(),
            capture_rms: Self::silent(),
            playback_rms: Self::silent(),
            load_percent: 0.0,
            clipped_samples: 0,
            error,
        }
    }
}

/// Interpret a CamillaDSP per-channel level reply (`[L, R]` dB floats) as a
/// stereo pair. Missing or non-numeric values fall back to silence.
fn stereo_levels(value: Option<Value>) -> Vec<f64> {
    let values = value.as_ref().and_then(Value::as_array);
    (0..2)
        .map(|i| {
            values
                .and_then(|a| a.get(i))
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
                // CamillaDSP reports silence as -1000 dB; the UI shows -120.
                .map(|v| v.max(SILENT_DB))
                .unwrap_or(SILENT_DB)
        })
        .collect()
}

pub struct CamillaEngine {
    child: Option<Child>,
    ws: Option<WsStream>,
    running_io: Option<IoConfig>,
    last_error: Option<String>,
    log_path: PathBuf,
}

pub const DEFAULT_CAMILLA_WS_PORT: u16 = 1234;

async fn free_local_port() -> Result<u16, String> {
    if let Ok(listener) = TcpListener::bind(("127.0.0.1", DEFAULT_CAMILLA_WS_PORT)).await {
        if let Ok(addr) = listener.local_addr() {
            return Ok(addr.port());
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("binding a free port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("reading bound port: {e}"))?
        .port();
    Ok(port)
}

/// Last `count` lines of a log file, joined for a single-line error.
fn log_tail(path: &Path, count: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(count);
            lines[start..].join(" | ")
        }
        Err(_) => String::from("no log output"),
    }
}

/// Interpret a CamillaDSP websocket reply of the form
/// `{"<Command>": {"result": "Ok"|"Error", "value": ...}}`.
fn parse_reply(reply: &Value) -> Result<Value, String> {
    let inner = reply
        .as_object()
        .and_then(|o| o.values().next())
        .ok_or_else(|| format!("unexpected reply: {reply}"))?;
    match inner.get("result").and_then(Value::as_str) {
        Some("Ok") => Ok(inner.get("value").cloned().unwrap_or(Value::Null)),
        _ => Err(match inner.get("value") {
            Some(Value::String(message)) => message.clone(),
            _ => format!("CamillaDSP rejected the command: {inner}"),
        }),
    }
}

async fn exchange(ws: &mut WsStream, command: &Value) -> Result<Value, String> {
    ws.send(Message::Text(command.to_string().into()))
        .await
        .map_err(|e| format!("websocket send failed: {e}"))?;
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                let text: &str = &text;
                let reply: Value =
                    serde_json::from_str(text).map_err(|e| format!("bad reply JSON: {e}"))?;
                return parse_reply(&reply);
            }
            Some(Ok(Message::Close(_))) | None => {
                return Err("websocket closed by CamillaDSP".to_string());
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(format!("websocket error: {e}")),
        }
    }
}

impl CamillaEngine {
    pub fn new(log_path: PathBuf) -> Self {
        Self {
            child: None,
            ws: None,
            running_io: None,
            last_error: None,
            log_path,
        }
    }

    /// Detect a CamillaDSP process that has exited on its own.
    fn reap_exited(&mut self) {
        let exit = match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => Some(status.to_string()),
                _ => None,
            },
            None => None,
        };
        if let Some(status) = exit {
            self.child = None;
            self.ws = None;
            self.running_io = None;
            self.last_error = Some(format!(
                "CamillaDSP exited ({status}): {}",
                log_tail(&self.log_path, 12)
            ));
        }
    }

    pub fn is_running(&mut self) -> bool {
        self.reap_exited();
        self.child.is_some() && self.ws.is_some()
    }

    pub fn running_io(&mut self) -> Option<IoConfig> {
        self.reap_exited();
        if self.is_running() {
            self.running_io.clone()
        } else {
            None
        }
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.clone()
    }

    /// Start CamillaDSP with `config_path` and connect to its websocket.
    pub async fn start(&mut self, binary: &Path, config_path: &Path, io: &IoConfig) -> Result<(), String> {
        self.stop().await;
        if let Some(parent) = self.log_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("creating log dir: {e}"))?;
        }
        let port = free_local_port().await?;
        let child = Command::new(binary)
            .arg("-p")
            .arg(port.to_string())
            .arg("-a")
            .arg("127.0.0.1")
            .arg("-o")
            .arg(&self.log_path)
            .arg(config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("starting {}: {e}", binary.display()))?;
        self.child = Some(child);

        let url = format!("ws://127.0.0.1:{port}");
        let mut last_error = String::from("no attempt made");
        for _ in 0..60 {
            self.reap_exited();
            if self.child.is_none() {
                let message = self
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "CamillaDSP exited during startup".to_string());
                return Err(message);
            }
            match connect_async(url.as_str()).await {
                Ok((ws, _)) => {
                    self.ws = Some(ws);
                    self.running_io = Some(io.clone());
                    self.last_error = None;
                    return Ok(());
                }
                Err(e) => {
                    last_error = e.to_string();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        self.stop().await;
        Err(format!("CamillaDSP websocket did not come up: {last_error}"))
    }

    pub async fn stop(&mut self) {
        self.ws = None;
        self.running_io = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
    }

    /// Hot-swap the configuration (`SetConfigJson`). Devices and sample rate
    /// must match the running configuration; callers restart otherwise.
    pub async fn reload(&mut self, config: &Value) -> Result<(), String> {
        if !self.is_running() {
            return Err("engine is not running".to_string());
        }
        let config_text = config.to_string();
        let command = json!({"SetConfigJson": config_text});
        self.request(&command).await.map(|_| ())
    }

    async fn request(&mut self, command: &Value) -> Result<Value, String> {
        let ws = self.ws.as_mut().ok_or_else(|| "websocket not connected".to_string())?;
        let result = exchange(ws, command).await;
        if let Err(e) = &result {
            // A transport error means the connection is unusable.
            if !e.starts_with("CamillaDSP") {
                self.ws = None;
            }
        }
        result
    }

    /// Query live state and signal levels. Never fails: problems are
    /// reported in `error` with `running` cleared.
    pub async fn meters(&mut self) -> LiveMeters {
        if !self.is_running() {
            return LiveMeters::stopped(self.last_error.clone());
        }
        let (sample_rate, chunksize) = self
            .running_io
            .as_ref()
            .map(|io| (io.sample_rate, io.chunk_size))
            .unwrap_or((crate::state::DEFAULT_SAMPLE_RATE, crate::state::DEFAULT_CHUNKSIZE));
        let state = self
            .request(&json!("GetState"))
            .await
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "Unknown".to_string());
        let capture_peak = stereo_levels(self.request(&json!("GetCaptureSignalPeak")).await.ok());
        let playback_peak = stereo_levels(self.request(&json!("GetPlaybackSignalPeak")).await.ok());
        let capture_rms = stereo_levels(self.request(&json!("GetCaptureSignalRms")).await.ok());
        let playback_rms = stereo_levels(self.request(&json!("GetPlaybackSignalRms")).await.ok());
        let load_percent = self
            .request(&json!("GetProcessingLoad"))
            .await
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let clipped_samples = self
            .request(&json!("GetClippedSamples"))
            .await
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        if self.ws.is_none() {
            let mut meters = LiveMeters::stopped(Some(
                self.last_error
                    .clone()
                    .unwrap_or_else(|| "lost connection to CamillaDSP".to_string()),
            ));
            meters.running = false;
            return meters;
        }
        LiveMeters {
            running: true,
            state,
            sample_rate,
            chunksize,
            capture_peak,
            playback_peak,
            capture_rms,
            playback_rms,
            load_percent,
            clipped_samples,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::AudioDeviceInfo;

    fn io(backend: &str, loopback: bool) -> IoConfig {
        IoConfig {
            capture: AudioDeviceInfo {
                id: "cap".into(),
                label: "cap".into(),
                backend: backend.into(),
                is_loopback: loopback,
                channels: 2,
            },
            playback: AudioDeviceInfo {
                id: "play".into(),
                label: "play".into(),
                backend: backend.into(),
                is_loopback: false,
                channels: 2,
            },
            sample_rate: 48000,
            exclusive: false,
            chunk_size: 1024,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hdsp-camilla-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bypass_keeps_level_matching_preamp_and_omits_processing() {
        let dir = temp_dir("bypass");
        let mut dsp = DspSettings::default();
        dsp.crossfeed_enabled = true;
        dsp.preamp_db = -6.0;
        dsp.eq_bands = vec![EqBand::new("bell", 1000.0, 6.0, 1.0)];
        let active_cfg = build_config(&io("pulse", false), &dsp, &dir).unwrap();
        let active_gain = active_cfg["filters"]["preamp_gain"]["parameters"]["gain"]
            .as_f64()
            .unwrap();

        dsp.master_bypass = true;
        let bypass_cfg = build_config(&io("pulse", false), &dsp, &dir).unwrap();
        let pipeline = bypass_cfg["pipeline"].as_array().unwrap();
        assert_eq!(pipeline.len(), 1, "only level-matching preamp_gain remains: {pipeline:?}");
        assert_eq!(pipeline[0]["names"], json!(["preamp_gain"]));
        let bypass_gain = bypass_cfg["filters"]["preamp_gain"]["parameters"]["gain"]
            .as_f64()
            .unwrap();
        assert_eq!(bypass_gain, active_gain, "A/B bypass must match processed gain offset");
        assert!(bypass_cfg["mixers"].as_object().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_chain_has_expected_stages_in_order() {
        let dir = temp_dir("full");
        let mut dsp = DspSettings::default();
        dsp.crossfeed_enabled = true;
        dsp.eq_bands = vec![
            EqBand::new("bell", 1000.0, 6.0, 1.0),
            EqBand::new("low_shelf", 100.0, 3.0, 0.7),
            EqBand::new("highpass", 30.0, 0.0, 0.7),
        ];
        let config = build_config(&io("pulse", false), &dsp, &dir).unwrap();
        let pipeline = config["pipeline"].as_array().unwrap();
        let names: Vec<String> = pipeline
            .iter()
            .map(|s| {
                s.get("names")
                    .map(|n| n.to_string())
                    .or_else(|| s.get("name").map(|n| n.to_string()))
                    .unwrap_or_default()
            })
            .collect();
        assert!(names[0].contains("preamp_gain"), "preamp first: {names:?}");
        assert!(names.iter().any(|n| n.contains("crossfeed_mid_filter")));
        assert!(names.iter().any(|n| n.contains("parametric_eq_band_2")));

        let filters = config["filters"].as_object().unwrap();
        assert_eq!(filters["parametric_eq_band_0"]["parameters"]["type"], "Peaking");
        assert_eq!(filters["parametric_eq_band_1"]["parameters"]["type"], "Lowshelf");
        assert_eq!(filters["parametric_eq_band_2"]["parameters"]["type"], "Highpass");
        assert!(filters["parametric_eq_band_2"]["parameters"].get("gain").is_none());
        assert!(dir.join("generated_fir/mid_filter.txt").is_file());
        assert!(dir.join("generated_fir/side_filter.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn high_rate_sources_resample_to_the_ku100_base_rate() {
        let dir = temp_dir("resample");
        let mut route = io("pulse", false);
        let dsp = DspSettings::default();

        route.sample_rate = 88_200;
        let cfg = build_config(&route, &dsp, &dir).unwrap();
        assert_eq!(cfg["devices"]["samplerate"], 44_100);
        assert_eq!(cfg["devices"]["capture_samplerate"], 88_200);
        assert_eq!(cfg["devices"]["resampler"]["type"], "Synchronous");

        route.sample_rate = 96_000;
        let cfg = build_config(&route, &dsp, &dir).unwrap();
        assert_eq!(cfg["devices"]["samplerate"], 48_000);
        assert_eq!(cfg["devices"]["capture_samplerate"], 96_000);
        assert_eq!(cfg["devices"]["resampler"]["type"], "Synchronous");

        route.sample_rate = 32_000;
        let cfg = build_config(&route, &dsp, &dir).unwrap();
        assert_eq!(cfg["devices"]["samplerate"], 44_100);
        assert_eq!(cfg["devices"]["capture_samplerate"], 32_000);
        assert_eq!(cfg["devices"]["resampler"]["type"], "AsyncSinc");

        route.sample_rate = 48_000;
        let cfg = build_config(&route, &dsp, &dir).unwrap();
        assert_eq!(cfg["devices"]["samplerate"], 48_000);
        assert!(cfg["devices"].get("capture_samplerate").is_none());
        assert!(cfg["devices"].get("resampler").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn device_blocks_match_backend() {
        let dir = temp_dir("devices");
        let wasapi = build_config(&io("wasapi", true), &DspSettings::default(), &dir).unwrap();
        assert_eq!(wasapi["devices"]["capture"]["type"], "Wasapi");
        assert_eq!(wasapi["devices"]["capture"]["loopback"], true);
        assert_eq!(wasapi["devices"]["playback"]["format"], "F32_LE");
        let alsa = build_config(&io("alsa", false), &DspSettings::default(), &dir).unwrap();
        assert_eq!(alsa["devices"]["playback"]["type"], "Alsa");
        assert_eq!(alsa["devices"]["playback"]["format"], "S32_LE");
        assert!(build_config(&io("jack", false), &DspSettings::default(), &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hardware_playback_enables_rate_adjust_with_async_capture() {
        let dir = temp_dir("rate-adjust");
        let dsp = DspSettings::default();
        for backend in ["alsa", "wasapi"] {
            for rate in [48_000u32, 88_200] {
                let mut route = io(backend, false);
                route.sample_rate = rate;
                let cfg = build_config(&route, &dsp, &dir).unwrap();
                let devices = &cfg["devices"];
                assert_eq!(devices["enable_rate_adjust"], json!(true), "{backend} {rate}");
                assert_eq!(devices["target_level"], json!(route.chunk_size * 2), "{backend} {rate}");
                assert_eq!(devices["adjust_period"], json!(3), "{backend} {rate}");
                assert_eq!(devices["silence_timeout"], json!(0), "{backend} {rate}");
                assert_eq!(devices["capture_samplerate"], json!(rate), "{backend} {rate}");
                assert_eq!(devices["resampler"]["type"], json!("AsyncSinc"), "{backend} {rate}");
                assert_eq!(devices["resampler"]["profile"], json!("Accurate"), "{backend} {rate}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pulse_playback_keeps_synchronous_resampling() {
        let dir = temp_dir("pulse-rate");
        let dsp = DspSettings::default();
        let mut route = io("pulse", false);
        for (rate, resampler) in [(88_200u32, "Synchronous"), (96_000, "Synchronous")] {
            route.sample_rate = rate;
            let cfg = build_config(&route, &dsp, &dir).unwrap();
            assert_eq!(cfg["devices"]["enable_rate_adjust"], json!(false));
            assert!(cfg["devices"].get("target_level").is_none());
            assert_eq!(cfg["devices"]["capture_samplerate"], json!(rate));
            assert_eq!(cfg["devices"]["resampler"]["type"], json!(resampler));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn composite_preamp_evaluates_crossfeed_and_eq_together() {
        let dir = temp_dir("headroom");
        let mut route = io("pulse", false);
        route.sample_rate = 44_100;

        // 1. Crossfeed alone (EQ disabled): preamp_gain reserves crossfeed headroom (~-3.84 dB).
        let mut dsp = DspSettings::default();
        dsp.crossfeed_enabled = true;
        dsp.crossfeed_angle_deg = 60.0;
        dsp.crossfeed_mid_boost_db = 3.0;
        dsp.eq_enabled = false;
        let cfg_cf_only = build_config(&route, &dsp, &dir).unwrap();
        let cf_preamp = cfg_cf_only["filters"]["preamp_gain"]["parameters"]["gain"]
            .as_f64()
            .unwrap();
        assert!(cf_preamp < -3.0 && cf_preamp > -4.5, "crossfeed-only preamp: {cf_preamp}");
        let mapping = &cfg_cf_only["mixers"]["crossfeed_from_mid_side"]["mapping"];
        assert_eq!(mapping[0]["sources"][0]["gain"], json!(0.0));
        assert_eq!(mapping[1]["sources"][1]["inverted"], json!(true));

        // 2. Non-overlapping +3.5 dB bass boost at 31 Hz (where crossfeed is ~0 dB)
        // must NOT stack on top of the ~1.8 kHz crossfeed peak.
        dsp.eq_enabled = true;
        dsp.eq_bands = vec![EqBand::new("bell", 31.0, 3.5, 1.41)];
        let cfg_non_overlap = build_config(&route, &dsp, &dir).unwrap();
        let non_overlap_preamp = cfg_non_overlap["filters"]["preamp_gain"]["parameters"]["gain"]
            .as_f64()
            .unwrap();
        assert!(
            (non_overlap_preamp - cf_preamp).abs() < 0.25,
            "non-overlapping bass EQ ({non_overlap_preamp}) should not double-penalize crossfeed ({cf_preamp})"
        );

        // 3. Overlapping +4.0 dB boost at 660 Hz (on top of the 60° crossfeed peak) MUST add.
        dsp.eq_bands = vec![EqBand::new("bell", 660.0, 4.0, 1.41)];
        let cfg_overlap = build_config(&route, &dsp, &dir).unwrap();
        let overlap_preamp = cfg_overlap["filters"]["preamp_gain"]["parameters"]["gain"]
            .as_f64()
            .unwrap();
        assert!(
            overlap_preamp < cf_preamp - 3.5,
            "overlapping 660 Hz EQ ({overlap_preamp}) must add to crossfeed ({cf_preamp})"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stereo_levels_parses_channel_arrays_and_falls_back_to_silence() {
        assert_eq!(stereo_levels(Some(json!([-12.5, -14.0]))), vec![-12.5, -14.0]);
        assert_eq!(stereo_levels(Some(json!([-1.0]))), vec![-1.0, SILENT_DB]);
        assert_eq!(stereo_levels(Some(json!(-3.0))), vec![SILENT_DB, SILENT_DB]);
        assert_eq!(stereo_levels(None), vec![SILENT_DB, SILENT_DB]);
        assert_eq!(stereo_levels(Some(json!([null, -6.0]))), vec![SILENT_DB, -6.0]);
        assert_eq!(stereo_levels(Some(json!([-1000.0, -6.0]))), vec![SILENT_DB, -6.0]);
    }

    #[test]
    fn eq_disabled_leaves_bands_out_of_the_pipeline() {
        let dir = temp_dir("eq-off");
        let dsp = DspSettings {
            eq_enabled: false,
            crossfeed_enabled: false,
            preamp_db: -2.0,
            auto_preamp: false,
            ..DspSettings::default()
        };
        let config = build_config(&io("pulse", false), &dsp, &dir).unwrap();
        let filters = config["filters"].as_object().unwrap();
        assert!(filters.keys().all(|k| !k.starts_with("parametric_eq_band_")));
        let pipeline = config["pipeline"].as_array().unwrap();
        assert_eq!(pipeline.len(), 1, "only the preamp stage remains: {pipeline:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ui_band_types_map_to_camilla_filters() {
        let mut band = EqBand::new("high_pass", 80.0, 0.0, 0.7);
        assert_eq!(band.camilla_type(), "Highpass");
        band.band_type = "low_pass".into();
        assert_eq!(band.camilla_type(), "Lowpass");
        band.band_type = "high_shelf".into();
        assert_eq!(band.camilla_type(), "Highshelf");
    }

    #[test]
    fn parse_reply_handles_ok_and_error() {
        assert_eq!(
            parse_reply(&json!({"GetState": {"result": "Ok", "value": "Running"}})).unwrap(),
            json!("Running")
        );
        assert!(parse_reply(&json!({"SetConfigJson": {"result": "Error", "value": "bad"}})).is_err());
    }

    #[test]
    fn live_pulse_virtual_sink_starts_and_hot_reloads() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let devices = crate::devices::enumerate_devices();
            let Some(cap) = devices.capture.iter().find(|d| d.id == "headphone_dsp_sink.monitor").cloned() else {
                return;
            };
            let Some(play) = devices.playback.iter().find(|d| d.backend == "Pulse").cloned() else {
                return;
            };
            let io = IoConfig {
                capture: cap,
                playback: play,
                sample_rate: 48000,
                exclusive: false,
                chunk_size: 2048,
            };
            let dir = temp_dir("live-pulse");
            let mut dsp = DspSettings::default();
            dsp.crossfeed_enabled = true;
            dsp.crossfeed_angle_deg = 35.0;
            dsp.crossfeed_mid_boost_db = 3.0;
            dsp.eq_enabled = true;
            dsp.eq_bands = vec![
                EqBand::new("low_shelf", 105.0, 4.0, 0.71),
                EqBand::new("bell", 1000.0, -2.0, 1.4),
            ];
            let cfg = build_config(&io, &dsp, &dir).unwrap();
            let cfg_path = dir.join("active.yml");
            write_config_yaml(&cfg_path, &cfg).unwrap();
            let bin = sidecar_path().unwrap();
            let mut engine = CamillaEngine::new(dir.join("camilla.log"));
            engine.start(&bin, &cfg_path, &io).await.unwrap();

            let m = engine.meters().await;
            assert!(m.running, "engine should be running: {:?}", m);
            assert_eq!(m.capture_peak.len(), 2);
            assert_eq!(m.playback_peak.len(), 2);

            dsp.crossfeed_angle_deg = 45.0;
            dsp.eq_bands[0].gain_db = 5.5;
            let cfg2 = build_config(&io, &dsp, &dir).unwrap();
            engine.reload(&cfg2).await.unwrap();

            engine.stop().await;
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
