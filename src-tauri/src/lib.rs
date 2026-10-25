//! Headphone DSP backend: Tauri commands that connect the vanilla-JS UI to the
//! DSP model, device discovery, profiles and the CamillaDSP engine.
//!
//! Every command that changes state returns the full [`AppStateResponse`],
//! which is what `ui/app.js` renders from.

pub mod camilla;
pub mod crossfeed;
pub mod devices;
pub mod peq;
pub mod state;

use serde::Serialize;
use tauri::{Manager, RunEvent};
use tokio::sync::Mutex;

use crate::camilla::{build_config, sidecar_path, write_config_yaml, CamillaEngine, LiveMeters};
use crate::crossfeed::CrossfeedPreview;
use crate::devices::{enumerate_devices, AudioDeviceInfo, DeviceList, LINUX_VIRTUAL_SINK_MONITOR};
use crate::peq::{export_autoeq as export_eq_text, parse_autoeq_text, PeqResponse};
use crate::state::{compute_curves, AppSettings, IoConfig, Store, CUSTOM_PROFILE_LABEL, DEFAULT_CHUNKSIZE};

const SAMPLE_RATE_MIN: u32 = 8_000;
const SAMPLE_RATE_MAX: u32 = 384_000;
/// Active-profile label after an AutoEQ import, matching the UI.
const IMPORTED_PROFILE_LABEL: &str = "Imported AutoEQ";

/// Everything behind the single async mutex.
pub struct Core {
    store: Store,
    settings: AppSettings,
    devices: Option<DeviceList>,
    engine: CamillaEngine,
    /// Counts `get_live_meters` calls; drives the periodic sample-rate poll.
    rate_poll_ticks: u8,
}

pub struct AppState {
    core: Mutex<Core>,
}

/// Full UI snapshot. Returned by every state-changing command.
#[derive(Debug, Clone, Serialize)]
pub struct AppStateResponse {
    pub settings: AppSettings,
    pub capture_devices: Vec<AudioDeviceInfo>,
    pub playback_devices: Vec<AudioDeviceInfo>,
    pub profiles: Vec<String>,
    pub peq_preview: PeqResponse,
    pub crossfeed_preview: CrossfeedPreview,
    pub meters: LiveMeters,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportAutoEqResponse {
    pub text: String,
}

fn validate_io(io: &IoConfig) -> Result<(), String> {
    if io.capture.id.trim().is_empty() {
        return Err("select a capture device".to_string());
    }
    if io.playback.id.trim().is_empty() {
        return Err("select a playback device".to_string());
    }
    if !(SAMPLE_RATE_MIN..=SAMPLE_RATE_MAX).contains(&io.sample_rate) {
        return Err(format!(
            "sample rate {} Hz is outside {}-{} Hz",
            io.sample_rate, SAMPLE_RATE_MIN, SAMPLE_RATE_MAX
        ));
    }
    if io.chunk_size == 0 {
        return Err("chunk size must be positive".to_string());
    }
    Ok(())
}

/// A device that should not be offered as the default playback target.
fn is_virtual(device: &AudioDeviceInfo) -> bool {
    device.is_loopback || device.id.starts_with("headphone_dsp")
}

impl Core {
    fn save(&mut self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        if !self.settings.hw_volume_bypass {
            if let Some(vol) = crate::devices::read_linux_virtual_sink_volume() {
                self.settings.virtual_sink_volume = Some(vol);
            }
        }
        self.store.save_settings(&self.settings)
    }

    #[cfg(target_os = "linux")]
    fn apply_volume_mode(&mut self, was_bypass: bool) {
        if self.settings.hw_volume_bypass {
            if !was_bypass {
                if let Some(vol) = crate::devices::read_linux_virtual_sink_volume() {
                    self.settings.virtual_sink_volume = Some(vol);
                }
            }
            crate::devices::set_linux_virtual_sink_volume(100);
        } else if was_bypass {
            if let Some(vol) = self.settings.virtual_sink_volume {
                crate::devices::set_linux_virtual_sink_volume(vol);
            }
        }
    }

    /// First-run defaults: fill empty device selections from the scan.
    /// Capture prefers the virtual sink monitor, then any loopback source.
    /// Playback takes the first output that is not a virtual device.
    /// Returns true when anything changed.
    fn ensure_device_defaults(&mut self) -> bool {
        let Some(devices) = &self.devices else {
            return false;
        };
        let mut changed = false;
        let capture_valid = !self.settings.capture_device.trim().is_empty()
            && devices
                .capture
                .iter()
                .any(|d| d.id == self.settings.capture_device);
        if !capture_valid {
            let pick = devices
                .capture
                .iter()
                .find(|d| d.id == LINUX_VIRTUAL_SINK_MONITOR)
                .or_else(|| devices.capture.iter().find(|d| d.is_loopback))
                .or_else(|| devices.capture.first());
            if let Some(device) = pick {
                self.settings.capture_device = device.id.clone();
                self.settings.capture_backend = device.backend.clone();
                self.settings.capture_loopback = device.is_loopback;
                changed = true;
            }
        }
        if self.settings.playback_device.trim().is_empty() {
            if let Some(device) = devices.playback.iter().find(|d| !is_virtual(d)) {
                self.settings.playback_device = device.id.clone();
                self.settings.playback_backend = device.backend.clone();
                changed = true;
            }
        }
        changed
    }

    /// Start CamillaDSP for the current settings (stopping any running instance).
    async fn launch(&mut self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            crate::devices::maximize_linux_output_gains(
                &self.settings.playback_backend,
                &self.settings.playback_device,
            );
            if self.settings.hw_volume_bypass {
                crate::devices::set_linux_virtual_sink_volume(100);
            }
        }
        let io = self.settings.io_config();
        validate_io(&io)?;
        let config = build_config(&io, &self.settings.dsp(), &self.store.runtime_dir())?;
        let config_path = self.store.active_config_path();
        write_config_yaml(&config_path, &config)?;
        let binary = sidecar_path()?;
        self.engine.start(&binary, &config_path, &io).await
    }

    /// Push the current DSP settings into the running pipeline, if any.
    /// Uses the route the engine is running with, so a route change is never
    /// applied by reload.
    async fn hot_push(&mut self) -> Result<(), String> {
        let Some(running_io) = self.engine.running_io() else {
            return Ok(());
        };
        let config = build_config(&running_io, &self.settings.dsp(), &self.store.runtime_dir())?;
        write_config_yaml(&self.store.active_config_path(), &config)?;
        self.engine.reload(&config).await
    }

    /// Apply a settings change to the engine. A changed IO route restarts
    /// CamillaDSP; otherwise the DSP settings are hot-reloaded. No-op when the
    /// engine is stopped.
    async fn apply_to_engine(&mut self) -> Result<(), String> {
        let Some(running_io) = self.engine.running_io() else {
            return Ok(());
        };
        if running_io != self.settings.io_config() {
            self.launch().await
        } else {
            self.hot_push().await
        }
    }

    async fn view(&mut self) -> AppStateResponse {
        let dsp = self.settings.dsp();
        let (peq_preview, crossfeed_preview) = compute_curves(&dsp, self.settings.sample_rate);
        let mut meters = self.engine.meters().await;
        meters.sample_rate = self.settings.sample_rate;
        meters.chunksize = self.settings.chunksize;
        let devices = self.devices.clone().unwrap_or_default();
        AppStateResponse {
            settings: self.settings.clone(),
            capture_devices: devices.capture,
            playback_devices: devices.playback,
            profiles: self.store.list_profiles(),
            peq_preview,
            crossfeed_preview,
            meters,
        }
    }
}

async fn scan_devices() -> Result<DeviceList, String> {
    tokio::task::spawn_blocking(enumerate_devices)
        .await
        .map_err(|e| format!("device enumeration failed: {e}"))
}

/// Capture and playback device ids: the route a detected sample rate applies to.
fn route_of(settings: &AppSettings) -> (String, String) {
    (settings.capture_device.clone(), settings.playback_device.clone())
}

/// Detect the active source's sample rate. Runs off the async runtime because
/// it queries audio tools or the host API.
async fn detect_rate_async(capture_device: String, playback_device: String) -> Option<u32> {
    tokio::task::spawn_blocking(move || crate::devices::detect_source_sample_rate(&capture_device, &playback_device))
        .await
        .ok()
        .flatten()
}

/// Scan devices and cache them, then apply first-run defaults and detect the
/// sample rate of the selected route.
async fn refresh_device_cache(state: &AppState) -> Result<(), String> {
    let first_scan = state.core.lock().await.devices.is_none();
    let list = scan_devices().await?;
    let mut core = state.core.lock().await;
    #[cfg(target_os = "linux")]
    if core.settings.hw_volume_bypass {
        crate::devices::set_linux_virtual_sink_volume(100);
    } else if first_scan {
        if let Some(vol) = core.settings.virtual_sink_volume {
            crate::devices::set_linux_virtual_sink_volume(vol);
        }
    }
    core.devices = Some(list);
    let mut changed = core.ensure_device_defaults();
    let route = route_of(&core.settings);
    drop(core);

    let detected = detect_rate_async(route.0.clone(), route.1.clone()).await;
    let mut core = state.core.lock().await;
    if core.settings.chunksize != DEFAULT_CHUNKSIZE {
        core.settings.chunksize = DEFAULT_CHUNKSIZE;
        changed = true;
    }
    if let Some(rate) = detected.filter(|_| route_of(&core.settings) == route) {
        if core.settings.sample_rate != rate {
            core.settings.sample_rate = rate;
            changed = true;
        }
    }
    if changed {
        core.save()?;
    }
    Ok(())
}

/// Full application state: settings, devices, profiles, curves and live meters.
#[tauri::command]
async fn get_app_state(state: tauri::State<'_, AppState>) -> Result<AppStateResponse, String> {
    let needs_scan = state.core.lock().await.devices.is_none();
    if needs_scan {
        refresh_device_cache(&state).await?;
    }
    let mut core = state.core.lock().await;
    Ok(core.view().await)
}

/// Re-scan capture and playback devices (and ensure the Linux virtual sink).
#[tauri::command]
async fn refresh_devices(state: tauri::State<'_, AppState>) -> Result<AppStateResponse, String> {
    refresh_device_cache(&state).await?;
    let mut core = state.core.lock().await;
    Ok(core.view().await)
}

/// Save the settings. A running engine is updated: a changed IO route
/// restarts CamillaDSP, otherwise the DSP is hot-reloaded. The sample rate
/// and chunk size are backend-owned: the values sent by the UI are ignored,
/// and a changed capture or playback device re-detects the sample rate.
#[tauri::command]
async fn update_dsp_settings(
    settings: AppSettings,
    state: tauri::State<'_, AppState>,
) -> Result<AppStateResponse, String> {
    let mut core = state.core.lock().await;
    let prev_vol = core.settings.virtual_sink_volume;
    #[allow(unused_variables)]
    let was_bypass = core.settings.hw_volume_bypass;
    let prev_rate = core.settings.sample_rate;
    let prev_route = route_of(&core.settings);
    core.settings = settings.sanitized();
    core.settings.eq_bands = crate::peq::normalize_geq_bands(&core.settings.eq_bands);
    core.settings.chunksize = DEFAULT_CHUNKSIZE;
    core.settings.auto_preamp = true;
    core.settings.preamp_db = 0.0;
    core.settings.sample_rate = prev_rate;
    if core.settings.virtual_sink_volume.is_none() {
        core.settings.virtual_sink_volume = prev_vol;
    }
    #[cfg(target_os = "linux")]
    core.apply_volume_mode(was_bypass);
    let route = route_of(&core.settings);
    if route != prev_route {
        drop(core);
        let detected = detect_rate_async(route.0.clone(), route.1.clone()).await;
        core = state.core.lock().await;
        if let Some(rate) = detected.filter(|_| route_of(&core.settings) == route) {
            core.settings.sample_rate = rate;
        }
    }
    core.save()?;
    core.apply_to_engine().await?;
    Ok(core.view().await)
}

/// Start CamillaDSP with these settings. The sample rate is detected from the
/// active source (falling back to the previous rate). If it is already running
/// with the same IO route, the DSP settings are hot-reloaded instead.
#[tauri::command]
async fn start_engine(
    settings: AppSettings,
    state: tauri::State<'_, AppState>,
) -> Result<AppStateResponse, String> {
    let mut core = state.core.lock().await;
    let prev_vol = core.settings.virtual_sink_volume;
    #[allow(unused_variables)]
    let was_bypass = core.settings.hw_volume_bypass;
    let prev_rate = core.settings.sample_rate;
    core.settings = settings.sanitized();
    core.settings.eq_bands = crate::peq::normalize_geq_bands(&core.settings.eq_bands);
    core.settings.chunksize = DEFAULT_CHUNKSIZE;
    core.settings.auto_preamp = true;
    core.settings.preamp_db = 0.0;
    core.settings.sample_rate = prev_rate;
    if core.settings.virtual_sink_volume.is_none() {
        core.settings.virtual_sink_volume = prev_vol;
    }
    #[cfg(target_os = "linux")]
    core.apply_volume_mode(was_bypass);
    let route = route_of(&core.settings);
    drop(core);

    let rate = detect_rate_async(route.0.clone(), route.1.clone())
        .await
        .unwrap_or(prev_rate);
    let mut core = state.core.lock().await;
    if route_of(&core.settings) == route {
        core.settings.sample_rate = rate;
    }
    core.save()?;
    match core.engine.running_io() {
        Some(running) if running == core.settings.io_config() => core.hot_push().await?,
        _ => core.launch().await?,
    }
    Ok(core.view().await)
}

#[tauri::command]
async fn stop_engine(state: tauri::State<'_, AppState>) -> Result<AppStateResponse, String> {
    let mut core = state.core.lock().await;
    core.engine.stop().await;
    let _ = core.save();
    Ok(core.view().await)
}

/// Live status and per-channel signal levels from CamillaDSP. While the
/// engine runs, the source sample rate is re-detected every ~2 s; a change
/// restarts CamillaDSP at the new rate.
#[tauri::command]
async fn get_live_meters(state: tauri::State<'_, AppState>) -> Result<LiveMeters, String> {
    let mut core = state.core.lock().await;
    core.rate_poll_ticks = core.rate_poll_ticks.wrapping_add(1);
    #[cfg(target_os = "linux")]
    if core.settings.hw_volume_bypass {
        if crate::devices::read_linux_virtual_sink_volume() != Some(100) {
            crate::devices::set_linux_virtual_sink_volume(100);
        }
    } else if let Some(vol) = crate::devices::read_linux_virtual_sink_volume() {
        if core.settings.virtual_sink_volume != Some(vol) {
            core.settings.virtual_sink_volume = Some(vol);
            let _ = core.store.save_settings(&core.settings);
        }
    }
    if core.engine.is_running() && core.rate_poll_ticks % 10 == 0 {
        let route = route_of(&core.settings);
        drop(core);
        let detected = detect_rate_async(route.0.clone(), route.1.clone()).await;
        core = state.core.lock().await;
        if let Some(new_rate) = detected {
            if core.engine.is_running()
                && route_of(&core.settings) == route
                && new_rate != core.settings.sample_rate
            {
                core.settings.sample_rate = new_rate;
                let _ = core.save();
                let _ = core.launch().await;
            }
        }
    }
    let mut meters = core.engine.meters().await;
    meters.sample_rate = core.settings.sample_rate;
    meters.chunksize = core.settings.chunksize;
    Ok(meters)
}

/// Save the current DSP settings as a named profile and make it active. The
/// saved profile always uses automatic anti-clip preamp.
#[tauri::command]
async fn save_profile(name: String, state: tauri::State<'_, AppState>) -> Result<AppStateResponse, String> {
    let mut core = state.core.lock().await;
    let mut dsp = core.settings.dsp();
    dsp.auto_preamp = true;
    dsp.preamp_db = 0.0;
    core.store.save_profile(&name, &dsp)?;
    core.settings.apply_dsp(&dsp);
    core.settings.active_profile = name.trim().to_string();
    core.save()?;
    Ok(core.view().await)
}

/// Apply a named profile's DSP settings (the IO route is kept), make it
/// active, and push the result to a running engine. Preamp is always
/// automatic anti-clip, whatever the profile file holds.
#[tauri::command]
async fn load_profile(name: String, state: tauri::State<'_, AppState>) -> Result<AppStateResponse, String> {
    let mut core = state.core.lock().await;
    let profile = core.store.load_profile(&name)?;
    core.settings.apply_dsp(&profile.dsp);
    core.settings.auto_preamp = true;
    core.settings.preamp_db = 0.0;
    core.settings.active_profile = profile.name;
    core.save()?;
    core.apply_to_engine().await?;
    Ok(core.view().await)
}

#[tauri::command]
async fn delete_profile(name: String, state: tauri::State<'_, AppState>) -> Result<AppStateResponse, String> {
    let mut core = state.core.lock().await;
    core.store.delete_profile(&name)?;
    if core.settings.active_profile == name.trim() {
        core.settings.active_profile = CUSTOM_PROFILE_LABEL.to_string();
        core.save()?;
    }
    Ok(core.view().await)
}

/// Parse EqualizerAPO / AutoEQ text and replace the EQ, which is enabled. The
/// result is always the 10-band Graphic EQ: parametric bands are fitted onto
/// it, and `GraphicEQ:` curves are fitted the same way. A `Preamp:` line is
/// validated but ignored: preamp is automatic anti-clip headroom computed from
/// the imported EQ curve.
#[tauri::command]
async fn import_autoeq(text: String, state: tauri::State<'_, AppState>) -> Result<AppStateResponse, String> {
    let (_preamp, bands) = parse_autoeq_text(&text)?;
    let mut core = state.core.lock().await;
    core.settings.eq_bands = bands.iter().map(|b| b.sanitized()).collect();
    core.settings.eq_enabled = true;
    core.settings.preamp_db = 0.0;
    core.settings.auto_preamp = true;
    core.settings.active_profile = IMPORTED_PROFILE_LABEL.to_string();
    core.settings = core.settings.sanitized();
    core.settings.eq_bands = crate::peq::normalize_geq_bands(&core.settings.eq_bands);
    core.save()?;
    core.apply_to_engine().await?;
    Ok(core.view().await)
}

/// The current EQ as EqualizerAPO text. The preamp written is the effective
/// one (what actually reaches the output), as in the UI.
#[tauri::command]
async fn export_autoeq(state: tauri::State<'_, AppState>) -> Result<ExportAutoEqResponse, String> {
    let core = state.core.lock().await;
    let dsp = core.settings.dsp();
    let (peq, _) = compute_curves(&dsp, core.settings.sample_rate);
    Ok(ExportAutoEqResponse {
        text: export_eq_text(peq.effective_preamp_db, &dsp.eq_bands),
    })
}

/// Set the native webview zoom factor (`0.5` .. `3.0`).
#[tauri::command]
fn set_webview_zoom(zoom: f64, window: tauri::WebviewWindow) -> Result<f64, String> {
    let clamped = if zoom.is_finite() {
        zoom.clamp(0.5, 3.0)
    } else {
        1.0
    };
    window
        .set_zoom(clamped)
        .map_err(|e| format!("setting webview zoom: {e}"))?;
    Ok(clamped)
}

fn compute_crossfeed_preview_sync(
    angle_deg: f64,
    mid_boost_db: f64,
    sample_rate: u32,
    enabled: bool,
) -> CrossfeedPreview {
    let dsp_rate = crate::crossfeed::ku100_base_sample_rate(sample_rate);
    let mut preview = crate::crossfeed::compute_crossfeed_preview(angle_deg, dsp_rate, mid_boost_db);
    if !enabled {
        let zeros = vec![0.0; preview.freq_hz.len()];
        preview.mid_db = zeros.clone();
        preview.side_db = zeros.clone();
        preview.ild_db = zeros;
    }
    preview
}

/// Real KU100 Mid/Side/ILD filter response for the given parameters.
/// Pure computation: does not lock the engine mutex or touch CamillaDSP.
#[tauri::command]
async fn get_crossfeed_preview(
    angle_deg: f64,
    mid_boost_db: f64,
    sample_rate: u32,
    enabled: bool,
) -> Result<CrossfeedPreview, String> {
    tokio::task::spawn_blocking(move || {
        compute_crossfeed_preview_sync(angle_deg, mid_boost_db, sample_rate, enabled)
    })
    .await
    .map_err(|e| format!("crossfeed preview failed: {e}"))
}

/// Origin of the only external link the app opens: the AutoEQ headphone search.
const AUTOEQ_ORIGIN: &str = "https://autoeq.app";

/// True for `https://autoeq.app` and paths under it, with no whitespace,
/// control characters, or shell metacharacters. The Windows launcher runs the
/// URL through `cmd`, where those characters would split or extend the command.
fn is_permitted_external_url(url: &str) -> bool {
    let origin_ok = url == AUTOEQ_ORIGIN || url.starts_with(&format!("{AUTOEQ_ORIGIN}/"));
    origin_ok
        && url
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '&' | '|' | '<' | '>' | '^' | '"' | '%' | '(' | ')' | '!'))
}

/// Open an AutoEQ link in the system browser. Any other URL is rejected
/// before a process is started.
#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    let trimmed = url.trim();
    if !is_permitted_external_url(trimmed) {
        return Err("only https://autoeq.app links are permitted".to_string());
    }
    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open")
        .arg(trimmed)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("opening browser: {e}"))?;
    #[cfg(target_os = "windows")]
    std::process::Command::new("cmd")
        .args(["/C", "start", "", trimmed])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("opening browser: {e}"))?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let root = dirs::config_dir()
                .ok_or("no user config directory available")?
                .join("headphone-dsp");
            let store = Store::new(root);
            store.ensure_layout()?;
            let settings = store.load_settings();
            let engine = CamillaEngine::new(store.log_path());
            app.manage(AppState {
                core: Mutex::new(Core {
                    store,
                    settings,
                    devices: None,
                    engine,
                    rate_poll_ticks: 0,
                }),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_state,
            refresh_devices,
            update_dsp_settings,
            start_engine,
            stop_engine,
            get_live_meters,
            save_profile,
            load_profile,
            delete_profile,
            import_autoeq,
            export_autoeq,
            set_webview_zoom,
            open_external_url,
            get_crossfeed_preview,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Headphone DSP");

    app.run(|app_handle, event| {
        if let RunEvent::Exit = event {
            let state = app_handle.state::<AppState>();
            tauri::async_runtime::block_on(async {
                let mut core = state.core.lock().await;
                let _ = core.save();
                core.engine.stop().await;
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DEFAULT_SAMPLE_RATE;

    fn device(id: &str, backend: &str, loopback: bool) -> AudioDeviceInfo {
        AudioDeviceInfo {
            id: id.into(),
            label: id.into(),
            backend: backend.into(),
            is_loopback: loopback,
            channels: 2,
        }
    }

    #[test]
    fn first_run_defaults_prefer_virtual_sink_and_real_output() {
        let mut core = Core {
            store: Store::new(std::env::temp_dir().join("hdsp-lib-test-unused")),
            settings: AppSettings::default(),
            devices: Some(DeviceList {
                capture: vec![
                    device("alsa_input.mic", "Pulse", false),
                    device("alsa_output.pci.monitor", "Pulse", true),
                    device(LINUX_VIRTUAL_SINK_MONITOR, "Pulse", true),
                ],
                playback: vec![
                    device("headphone_dsp_sink", "Pulse", false),
                    device("alsa_output.pci", "Pulse", false),
                    device("plughw:CARD=Generic,DEV=0", "Alsa", false),
                ],
                ..DeviceList::default()
            }),
            engine: CamillaEngine::new(std::env::temp_dir().join("hdsp-lib-test-log")),
            rate_poll_ticks: 0,
        };
        assert!(core.ensure_device_defaults());
        assert_eq!(core.settings.capture_device, LINUX_VIRTUAL_SINK_MONITOR);
        assert!(core.settings.capture_loopback);
        assert_eq!(core.settings.capture_backend, "Pulse");
        assert_eq!(core.settings.playback_device, "alsa_output.pci");
        assert_eq!(core.settings.playback_backend, "Pulse");
        assert!(!core.ensure_device_defaults(), "second pass changes nothing");
    }

    #[test]
    fn io_validation_rejects_bad_routes() {
        let mut settings = AppSettings {
            capture_device: "cap".into(),
            playback_device: "play".into(),
            ..AppSettings::default()
        };
        assert!(validate_io(&settings.io_config()).is_ok());
        settings.playback_device.clear();
        assert!(validate_io(&settings.io_config()).is_err());
        settings.playback_device = "play".into();
        settings.sample_rate = 1_000;
        assert!(validate_io(&settings.io_config()).is_err());
    }

    #[test]
    fn open_external_url_rejects_disallowed_urls() {
        for url in [
            "http://example.com",
            "file:///etc/passwd",
            "https://autoeq.app.evil.example",
            "https://autoeq.app.example/",
            "https://autoeq.app/?q=a&calc",
            "https://autoeq.app/ x",
            "",
        ] {
            assert_eq!(
                open_external_url(url.to_string()),
                Err("only https://autoeq.app links are permitted".to_string()),
                "url {url:?} must be rejected"
            );
        }
    }

    #[test]
    fn autoeq_links_are_the_only_permitted_external_urls() {
        assert!(is_permitted_external_url("https://autoeq.app"));
        assert!(is_permitted_external_url("https://autoeq.app/?q=hd650"));
        assert!(!is_permitted_external_url("http://autoeq.app"));
        assert!(!is_permitted_external_url("https://autoeq.app.example/"));
    }

    #[test]
    fn get_crossfeed_preview_follows_the_enabled_flag() {
        let on = compute_crossfeed_preview_sync(35.0, 3.0, 48_000, true);
        assert_eq!(on.freq_hz.len(), 256);
        for curve in [&on.mid_db, &on.side_db, &on.ild_db] {
            assert_eq!(curve.len(), 256);
            assert!(curve.iter().any(|v| v.abs() > 1e-6), "enabled curve must be non-zero");
        }

        let off = compute_crossfeed_preview_sync(35.0, 3.0, 48_000, false);
        assert_eq!(off.freq_hz, on.freq_hz);
        for curve in [&off.mid_db, &off.side_db, &off.ild_db] {
            assert!(curve.iter().all(|v| *v == 0.0), "disabled curve must be all zeros");
        }
    }

    #[test]
    fn response_serializes_with_ui_field_names() {
        let (peq, crossfeed) = compute_curves(&AppSettings::default().dsp(), DEFAULT_SAMPLE_RATE);
        let response = AppStateResponse {
            settings: AppSettings::default(),
            capture_devices: vec![],
            playback_devices: vec![],
            profiles: vec!["Default".into()],
            peq_preview: peq,
            crossfeed_preview: crossfeed,
            meters: LiveMeters::stopped(None),
        };
        let value = serde_json::to_value(&response).unwrap();
        for key in ["settings", "capture_devices", "playback_devices", "profiles", "peq_preview", "crossfeed_preview", "meters"] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
        assert_eq!(value["meters"]["capture_peak"], serde_json::json!([-120.0, -120.0]));
        assert_eq!(value["meters"]["state"], "Stopped");
        assert_eq!(value["settings"]["chunksize"], 512);
        assert_eq!(value["settings"]["eq_bands"][0]["band_type"], "bell");
        assert_eq!(
            serde_json::to_value(ExportAutoEqResponse { text: "x".into() }).unwrap(),
            serde_json::json!({"text": "x"})
        );
    }
}
