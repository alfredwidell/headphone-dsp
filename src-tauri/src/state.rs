//! Persistent settings, named profiles and the built-in starter presets.
//!
//! [`AppSettings`] is the flat object the UI sends and receives. It holds the
//! IO route (devices, backends, sample rate, chunk size) and the DSP chain.
//! [`DspSettings`] is the DSP subset: what a profile stores, and what the
//! CamillaDSP builder consumes.
//!
//! Layout under the user config directory (`~/.config/headphone-dsp` on
//! Linux, `%APPDATA%\headphone-dsp` on Windows):
//!
//! ```text
//! settings.json          last AppSettings (including active profile name)
//! profiles/<name>.json   one named DSP profile per file
//! profiles/.seeded       marker: built-in profiles have been written once
//! runtime/               generated CamillaDSP config, FIR files and log
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crossfeed::{
    coerce_crossfeed_angle_deg, coerce_crossfeed_mid_boost_db, compute_crossfeed_preview,
    CrossfeedPreview,
};
use crate::devices::{canonical_backend, AudioDeviceInfo, BACKEND_ALSA, BACKEND_PULSE};
use crate::peq::{
    compute_peq_response, default_geq_bands, normalize_geq_bands, EqBand, PeqResponse, MAX_BANDS,
};

pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
pub const DEFAULT_CHUNKSIZE: usize = 512;
/// First-run crossfeed angle and Mid boost, as the UI defaults them (the
/// Python port's own defaults differ: 30° and 9 dB).
pub const UI_DEFAULT_CROSSFEED_ANGLE_DEG: f64 = 35.0;
pub const UI_DEFAULT_CROSSFEED_MID_BOOST_DB: f64 = 3.0;
pub const DEFAULT_ACTIVE_PROFILE: &str = "Default";
/// Active-profile label after the active profile is deleted.
pub const CUSTOM_PROFILE_LABEL: &str = "Custom";
const CHUNKSIZE_MIN: usize = 32;
const CHUNKSIZE_MAX: usize = 65_536;
const PREAMP_MIN_DB: f64 = -18.0;
const PREAMP_MAX_DB: f64 = 12.0;
const PROFILE_NAME_MAX_CHARS: usize = 64;
const SEEDED_MARKER: &str = ".seeded";

/// DSP chain settings. Stored in profiles; consumed by the CamillaDSP builder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DspSettings {
    pub crossfeed_enabled: bool,
    pub crossfeed_angle_deg: f64,
    pub crossfeed_mid_boost_db: f64,
    pub preamp_db: f64,
    pub auto_preamp: bool,
    pub master_bypass: bool,
    pub eq_enabled: bool,
    pub eq_bands: Vec<EqBand>,
}

impl Default for DspSettings {
    fn default() -> Self {
        Self {
            crossfeed_enabled: true,
            crossfeed_angle_deg: UI_DEFAULT_CROSSFEED_ANGLE_DEG,
            crossfeed_mid_boost_db: UI_DEFAULT_CROSSFEED_MID_BOOST_DB,
            preamp_db: 0.0,
            auto_preamp: true,
            master_bypass: false,
            eq_enabled: true,
            eq_bands: default_geq_bands([3.5, 3.0, 1.5, 0.0, 0.0, 0.0, 0.0, -0.5, -1.5, -1.5]),
        }
    }
}

impl DspSettings {
    /// Clamp every field to its legal range and canonicalize band types.
    pub fn sanitized(&self) -> Self {
        Self {
            crossfeed_enabled: self.crossfeed_enabled,
            crossfeed_angle_deg: coerce_crossfeed_angle_deg(self.crossfeed_angle_deg),
            crossfeed_mid_boost_db: coerce_crossfeed_mid_boost_db(self.crossfeed_mid_boost_db),
            preamp_db: clamp_preamp(self.preamp_db),
            auto_preamp: self.auto_preamp,
            master_bypass: self.master_bypass,
            eq_enabled: self.eq_enabled,
            eq_bands: self
                .eq_bands
                .iter()
                .take(MAX_BANDS)
                .map(|b| b.sanitized())
                .collect(),
        }
    }
}

fn clamp_preamp(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(PREAMP_MIN_DB, PREAMP_MAX_DB)
    } else {
        0.0
    }
}

/// The flat settings object exchanged with the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub capture_backend: String,
    pub capture_device: String,
    pub capture_loopback: bool,
    pub playback_backend: String,
    pub playback_device: String,
    pub exclusive_mode: bool,
    #[serde(default)]
    pub hw_volume_bypass: bool,
    pub sample_rate: u32,
    #[serde(alias = "chunk_size")]
    pub chunksize: usize,
    pub master_bypass: bool,
    pub preamp_db: f64,
    pub auto_preamp: bool,
    pub crossfeed_enabled: bool,
    pub crossfeed_angle_deg: f64,
    pub crossfeed_mid_boost_db: f64,
    pub eq_enabled: bool,
    pub eq_bands: Vec<EqBand>,
    pub active_profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_sink_volume: Option<u8>,
}

impl Default for AppSettings {
    fn default() -> Self {
        let dsp = DspSettings::default();
        Self {
            capture_backend: BACKEND_PULSE.to_string(),
            capture_device: String::new(),
            capture_loopback: true,
            playback_backend: BACKEND_ALSA.to_string(),
            playback_device: String::new(),
            exclusive_mode: false,
            hw_volume_bypass: false,
            sample_rate: DEFAULT_SAMPLE_RATE,
            chunksize: DEFAULT_CHUNKSIZE,
            master_bypass: dsp.master_bypass,
            preamp_db: dsp.preamp_db,
            auto_preamp: dsp.auto_preamp,
            crossfeed_enabled: dsp.crossfeed_enabled,
            crossfeed_angle_deg: dsp.crossfeed_angle_deg,
            crossfeed_mid_boost_db: dsp.crossfeed_mid_boost_db,
            eq_enabled: dsp.eq_enabled,
            eq_bands: dsp.eq_bands,
            active_profile: DEFAULT_ACTIVE_PROFILE.to_string(),
            virtual_sink_volume: None,
        }
    }
}

impl AppSettings {
    /// Clamp and canonicalize every field. Applied on every entry point.
    pub fn sanitized(&self) -> Self {
        let dsp = self.dsp().sanitized();
        let mut out = Self {
            capture_backend: canonical_backend(&self.capture_backend)
                .map(str::to_string)
                .unwrap_or_else(|| self.capture_backend.trim().to_string()),
            capture_device: self.capture_device.trim().to_string(),
            capture_loopback: self.capture_loopback,
            playback_backend: canonical_backend(&self.playback_backend)
                .map(str::to_string)
                .unwrap_or_else(|| self.playback_backend.trim().to_string()),
            playback_device: self.playback_device.trim().to_string(),
            exclusive_mode: self.exclusive_mode,
            hw_volume_bypass: self.hw_volume_bypass,
            sample_rate: self.sample_rate,
            chunksize: self.chunksize.clamp(CHUNKSIZE_MIN, CHUNKSIZE_MAX),
            master_bypass: dsp.master_bypass,
            preamp_db: dsp.preamp_db,
            auto_preamp: dsp.auto_preamp,
            crossfeed_enabled: dsp.crossfeed_enabled,
            crossfeed_angle_deg: dsp.crossfeed_angle_deg,
            crossfeed_mid_boost_db: dsp.crossfeed_mid_boost_db,
            eq_enabled: dsp.eq_enabled,
            eq_bands: dsp.eq_bands,
            active_profile: self.active_profile.trim().to_string(),
            virtual_sink_volume: self.virtual_sink_volume.map(|v| v.min(150)),
        };
        if out.active_profile.is_empty() {
            out.active_profile = DEFAULT_ACTIVE_PROFILE.to_string();
        }
        out
    }

    /// The DSP subset (what profiles store).
    pub fn dsp(&self) -> DspSettings {
        DspSettings {
            crossfeed_enabled: self.crossfeed_enabled,
            crossfeed_angle_deg: self.crossfeed_angle_deg,
            crossfeed_mid_boost_db: self.crossfeed_mid_boost_db,
            preamp_db: self.preamp_db,
            auto_preamp: self.auto_preamp,
            master_bypass: self.master_bypass,
            eq_enabled: self.eq_enabled,
            eq_bands: self.eq_bands.clone(),
        }
    }

    /// Replace the DSP fields with `dsp`, leaving the IO route untouched.
    pub fn apply_dsp(&mut self, dsp: &DspSettings) {
        self.crossfeed_enabled = dsp.crossfeed_enabled;
        self.crossfeed_angle_deg = dsp.crossfeed_angle_deg;
        self.crossfeed_mid_boost_db = dsp.crossfeed_mid_boost_db;
        self.preamp_db = dsp.preamp_db;
        self.auto_preamp = dsp.auto_preamp;
        self.master_bypass = dsp.master_bypass;
        self.eq_enabled = dsp.eq_enabled;
        self.eq_bands = dsp.eq_bands.clone();
    }

    /// The IO route. Equal routes compare equal, which is how the engine
    /// decides between a hot reload and a restart.
    pub fn io_config(&self) -> IoConfig {
        IoConfig {
            capture: AudioDeviceInfo {
                id: self.capture_device.clone(),
                label: self.capture_device.clone(),
                backend: canonical_backend(&self.capture_backend)
                    .unwrap_or(BACKEND_PULSE)
                    .to_string(),
                is_loopback: self.capture_loopback,
                channels: 2,
            },
            playback: AudioDeviceInfo {
                id: self.playback_device.clone(),
                label: self.playback_device.clone(),
                backend: canonical_backend(&self.playback_backend)
                    .unwrap_or(BACKEND_ALSA)
                    .to_string(),
                is_loopback: false,
                channels: 2,
            },
            sample_rate: self.sample_rate,
            exclusive: self.exclusive_mode,
            chunk_size: self.chunksize,
        }
    }
}

/// The resolved IO route handed to CamillaDSP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IoConfig {
    pub capture: AudioDeviceInfo,
    pub playback: AudioDeviceInfo,
    pub sample_rate: u32,
    /// Exclusive-mode playback (WASAPI only).
    pub exclusive: bool,
    pub chunk_size: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub dsp: DspSettings,
}

/// Filesystem layout. All paths are absolute.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.root.join("runtime")
    }

    pub fn active_config_path(&self) -> PathBuf {
        self.runtime_dir().join("active.yml")
    }

    pub fn log_path(&self) -> PathBuf {
        self.runtime_dir().join("camilladsp.log")
    }

    fn settings_path(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    fn profile_path(&self, name: &str) -> Result<PathBuf, String> {
        Ok(self.profiles_dir().join(format!("{}.json", profile_file_stem(name)?)))
    }

    /// Load persisted settings; a missing or corrupt file yields defaults.
    /// The chunk size is backend-owned: any stored value is replaced, which
    /// migrates older `settings.json` files to [`DEFAULT_CHUNKSIZE`]. Preamp is
    /// backend-owned too: it is always automatic anti-clip (`preamp_db = 0`).
    /// The EQ is canonicalized to the 10-band Graphic EQ.
    pub fn load_settings(&self) -> AppSettings {
        let mut settings = std::fs::read_to_string(self.settings_path())
            .ok()
            .and_then(|text| serde_json::from_str::<AppSettings>(&text).ok())
            .unwrap_or_default()
            .sanitized();
        settings.chunksize = DEFAULT_CHUNKSIZE;
        settings.auto_preamp = true;
        settings.preamp_db = 0.0;
        settings.eq_bands = normalize_geq_bands(&settings.eq_bands);
        settings
    }

    pub fn save_settings(&self, settings: &AppSettings) -> Result<(), String> {
        write_json(&self.settings_path(), settings)
    }

    /// Create the directory layout and write the built-in profiles once.
    /// Built-ins are not rewritten afterwards, so a deleted preset stays deleted.
    pub fn ensure_layout(&self) -> Result<(), String> {
        let profiles = self.profiles_dir();
        std::fs::create_dir_all(&profiles).map_err(|e| format!("creating {}: {e}", profiles.display()))?;
        std::fs::create_dir_all(self.runtime_dir()).map_err(|e| format!("creating runtime dir: {e}"))?;
        let marker = profiles.join(SEEDED_MARKER);
        if !marker.exists() {
            for profile in builtin_profiles() {
                if !self.profile_path(&profile.name)?.exists() {
                    self.write_profile(&profile)?;
                }
            }
            std::fs::write(&marker, "built-in profiles seeded\n")
                .map_err(|e| format!("writing seed marker: {e}"))?;
        }
        Ok(())
    }

    pub fn list_profiles(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.profiles_dir())
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .filter_map(|text| serde_json::from_str::<Profile>(&text).ok())
            .map(|profile| profile.name)
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    pub fn load_profile(&self, name: &str) -> Result<Profile, String> {
        let path = self.profile_path(name)?;
        let text = std::fs::read_to_string(&path)
            .map_err(|_| format!("profile '{}' does not exist", name.trim()))?;
        let profile: Profile =
            serde_json::from_str(&text).map_err(|e| format!("profile '{}' is corrupt: {e}", name.trim()))?;
        Ok(Profile {
            name: profile.name,
            dsp: canonical_dsp(&profile.dsp),
        })
    }

    pub fn save_profile(&self, name: &str, dsp: &DspSettings) -> Result<(), String> {
        let name = normalize_profile_name(name)?;
        self.write_profile(&Profile {
            name,
            dsp: canonical_dsp(dsp),
        })
    }

    fn write_profile(&self, profile: &Profile) -> Result<(), String> {
        let path = self.profile_path(&profile.name)?;
        std::fs::create_dir_all(self.profiles_dir()).map_err(|e| format!("creating profiles dir: {e}"))?;
        write_json(&path, profile)
    }

    pub fn delete_profile(&self, name: &str) -> Result<(), String> {
        let path = self.profile_path(name)?;
        std::fs::remove_file(&path).map_err(|_| format!("profile '{}' does not exist", name.trim()))
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| format!("serializing: {e}"))?;
    std::fs::write(path, text).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Sanitize DSP settings and canonicalize the EQ to the 10-band Graphic EQ.
fn canonical_dsp(dsp: &DspSettings) -> DspSettings {
    let mut out = dsp.sanitized();
    out.eq_bands = normalize_geq_bands(&out.eq_bands);
    out
}

fn normalize_profile_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("profile name must not be empty".to_string());
    }
    if trimmed.chars().count() > PROFILE_NAME_MAX_CHARS {
        return Err(format!("profile name must be at most {PROFILE_NAME_MAX_CHARS} characters"));
    }
    Ok(trimmed.to_string())
}

/// Filesystem-safe file stem for a profile name. Path separators and other
/// unsafe characters become '_' so a name can never escape `profiles/`.
fn profile_file_stem(name: &str) -> Result<String, String> {
    let normalized = normalize_profile_name(name)?;
    Ok(normalized
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '&' | '(' | ')' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .replace("..", "_"))
}

/// The three starter presets written on first run. Values mirror the UI's
/// built-in presets so the native and browser-preview modes agree.
pub fn builtin_profiles() -> Vec<Profile> {
    let base = DspSettings::default();
    vec![
        Profile {
            name: "Flat / Neutral".to_string(),
            dsp: DspSettings {
                crossfeed_enabled: false,
                eq_bands: default_geq_bands([0.0; crate::peq::GEQ_BAND_COUNT]),
                ..base.clone()
            },
        },
        Profile {
            name: "Warm Vocal & Sub-Bass".to_string(),
            dsp: DspSettings {
                eq_bands: default_geq_bands([4.0, 3.5, 2.0, 0.5, 0.0, 1.0, 1.5, 0.0, -1.5, -2.0]),
                ..base.clone()
            },
        },
        Profile {
            name: "HD 650 / 6XX Harman Target".to_string(),
            dsp: DspSettings {
                eq_bands: default_geq_bands([5.0, 4.0, 2.0, 0.0, 0.0, 0.0, -1.0, -2.0, 1.5, 2.0]),
                ..base
            },
        },
    ]
}

/// Preview curves for the DSP settings, evaluated at the DSP rate.
///
/// `sample_rate` is the source rate; the DSP runs at its KU100 base rate
/// (44.1 or 48 kHz), so the curves are evaluated there to match the pipeline.
/// Mirrors the UI's own preview model: master bypass shows the EQ curve but
/// no preamp, and disabled crossfeed reports zeros.
pub fn compute_curves(dsp: &DspSettings, sample_rate: u32) -> (PeqResponse, CrossfeedPreview) {
    let dsp_rate = crate::crossfeed::ku100_base_sample_rate(sample_rate);
    let eq = compute_peq_response(
        &dsp.eq_bands,
        dsp.preamp_db,
        dsp.auto_preamp,
        dsp_rate,
        dsp.eq_enabled,
    );

    let mut crossfeed = compute_crossfeed_preview(
        dsp.crossfeed_angle_deg,
        dsp_rate,
        dsp.crossfeed_mid_boost_db,
    );
    if !dsp.crossfeed_enabled {
        let zeros = vec![0.0; crossfeed.freq_hz.len()];
        crossfeed.mid_db = zeros.clone();
        crossfeed.side_db = zeros.clone();
        crossfeed.ild_db = zeros;
    }
    (eq, crossfeed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> Store {
        let root = std::env::temp_dir().join(format!("hdsp-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Store::new(root)
    }

    #[test]
    fn app_settings_defaults_match_ui_defaults() {
        let s = AppSettings::default();
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.chunksize, 512);
        assert_eq!(s.crossfeed_angle_deg, 35.0);
        assert_eq!(s.crossfeed_mid_boost_db, 3.0);
        assert!(s.eq_enabled);
        assert_eq!(s.active_profile, "Default");
        assert_eq!(s.eq_bands.len(), 10);
        assert_eq!(s.eq_bands[0].band_type, "bell");
        assert_eq!(s.eq_bands[0].freq_hz, 31.0);
    }

    #[test]
    fn app_settings_deserializes_ui_payload() {
        // Shape sent by ui/app.js (`settings` object), including the
        // capitalized backend names and the high_pass / low_pass types.
        let payload = r#"{
          "capture_backend": "Pulse", "capture_device": "headphone_dsp_sink.monitor",
          "capture_loopback": true, "playback_backend": "Alsa",
          "playback_device": "plughw:CARD=Generic_1,DEV=0", "exclusive_mode": false,
          "sample_rate": 44100, "chunksize": 1024, "master_bypass": false,
          "preamp_db": -3.5, "auto_preamp": true, "crossfeed_enabled": true,
          "crossfeed_angle_deg": 45, "crossfeed_mid_boost_db": 6, "eq_enabled": true,
          "eq_bands": [{"enabled": true, "band_type": "high_pass", "freq_hz": 30, "gain_db": 0, "q": 0.71}],
          "active_profile": "Warm Vocal & Sub-Bass"
        }"#;
        let s: AppSettings = serde_json::from_str(payload).unwrap();
        assert_eq!(s.capture_backend, "Pulse");
        assert_eq!(s.sample_rate, 44_100);
        assert_eq!(s.eq_bands[0].band_type, "high_pass");
        let io = s.io_config();
        assert_eq!(io.capture.backend, "Pulse");
        assert!(io.capture.is_loopback);
        assert_eq!(io.chunk_size, 1024);
    }

    #[test]
    fn chunk_size_alias_is_accepted() {
        let s: AppSettings = serde_json::from_str(r#"{"chunk_size": 4096}"#).unwrap();
        assert_eq!(s.chunksize, 4096);
    }

    #[test]
    fn backend_names_are_canonicalized_on_sanitize() {
        let s = AppSettings {
            capture_backend: "pulse".into(),
            playback_backend: "ALSA".into(),
            ..AppSettings::default()
        }
        .sanitized();
        assert_eq!(s.capture_backend, "Pulse");
        assert_eq!(s.playback_backend, "Alsa");
    }

    #[test]
    fn io_route_equality_drives_restart_decision() {
        let mut a = AppSettings {
            capture_device: "cap".into(),
            playback_device: "play".into(),
            ..AppSettings::default()
        };
        let before = a.io_config();
        a.preamp_db = -2.0;
        assert_eq!(a.io_config(), before, "DSP-only edits keep the route");
        a.sample_rate = 96_000;
        assert_ne!(a.io_config(), before, "sample-rate change restarts");
    }

    #[test]
    fn seeds_three_builtin_profiles_once() {
        let store = temp_store("seed");
        store.ensure_layout().unwrap();
        assert_eq!(
            store.list_profiles(),
            vec![
                "Flat / Neutral".to_string(),
                "HD 650 / 6XX Harman Target".to_string(),
                "Warm Vocal & Sub-Bass".to_string(),
            ]
        );
        store.delete_profile("Flat / Neutral").unwrap();
        store.ensure_layout().unwrap();
        assert!(!store.list_profiles().contains(&"Flat / Neutral".to_string()));
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn profile_save_load_delete_round_trip() {
        let store = temp_store("profile");
        store.ensure_layout().unwrap();
        let dsp = DspSettings {
            preamp_db: -4.25,
            eq_bands: vec![EqBand::new("notch", 60.0, 0.0, 8.0)],
            ..DspSettings::default()
        };
        store.save_profile("  My Cans/../x  ", &dsp).unwrap();
        assert!(store.list_profiles().contains(&"My Cans/../x".to_string()));
        let loaded = store.load_profile("My Cans/../x").unwrap();
        assert_eq!(
            loaded.dsp,
            DspSettings {
                eq_bands: crate::peq::normalize_geq_bands(&dsp.eq_bands),
                ..dsp.sanitized()
            },
            "saved EQ is canonicalized to the Graphic EQ"
        );
        store.delete_profile("My Cans/../x").unwrap();
        assert!(store.load_profile("My Cans/../x").is_err());
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn profile_names_are_validated() {
        assert!(normalize_profile_name("   ").is_err());
        assert!(normalize_profile_name(&"x".repeat(65)).is_err());
        assert!(!profile_file_stem("../../etc/passwd").unwrap().contains('/'));
    }

    #[test]
    fn settings_round_trip_and_corrupt_file_falls_back() {
        let store = temp_store("settings");
        store.ensure_layout().unwrap();
        let settings = AppSettings {
            crossfeed_enabled: false,
            active_profile: "Flat / Neutral".into(),
            ..AppSettings::default()
        };
        store.save_settings(&settings).unwrap();
        let back = store.load_settings();
        assert!(!back.crossfeed_enabled);
        assert_eq!(back.active_profile, "Flat / Neutral");

        std::fs::write(store.root().join("settings.json"), "{not json").unwrap();
        assert_eq!(store.load_settings(), AppSettings::default());
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn load_settings_forces_auto_anti_clip_preamp_and_chunksize() {
        let store = temp_store("autopreamp");
        store.ensure_layout().unwrap();
        let stored = AppSettings {
            preamp_db: -5.0,
            auto_preamp: false,
            chunksize: 2048,
            ..AppSettings::default()
        };
        store.save_settings(&stored).unwrap();
        let back = store.load_settings();
        assert_eq!(back.preamp_db, 0.0);
        assert!(back.auto_preamp);
        assert_eq!(back.chunksize, DEFAULT_CHUNKSIZE);
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn sanitize_clamps_out_of_range_values() {
        let s = AppSettings {
            crossfeed_angle_deg: 400.0,
            crossfeed_mid_boost_db: -3.0,
            preamp_db: 99.0,
            chunksize: 1,
            eq_bands: vec![EqBand {
                band_type: "unknown".into(),
                freq_hz: 1.0,
                gain_db: 80.0,
                q: 0.0,
                ..EqBand::default()
            }],
            ..AppSettings::default()
        }
        .sanitized();
        assert_eq!(s.crossfeed_angle_deg, 90.0);
        assert_eq!(s.crossfeed_mid_boost_db, 0.0);
        assert_eq!(s.preamp_db, 12.0);
        assert_eq!(s.chunksize, CHUNKSIZE_MIN);
        let band = &s.eq_bands[0];
        assert_eq!(band.band_type, "bell");
        assert_eq!(band.freq_hz, 20.0);
        assert_eq!(band.gain_db, 24.0);
        assert_eq!(band.q, 0.1);
    }

    #[test]
    fn eq_bands_are_capped_at_twelve() {
        let dsp = DspSettings {
            eq_bands: vec![EqBand::default(); 20],
            ..DspSettings::default()
        }
        .sanitized();
        assert_eq!(dsp.eq_bands.len(), MAX_BANDS);
    }

    #[test]
    fn master_bypass_curve_keeps_level_matching_preamp_and_disabled_crossfeed_is_zero() {
        let dsp = DspSettings {
            preamp_db: -6.0,
            auto_preamp: false,
            master_bypass: true,
            crossfeed_enabled: false,
            eq_bands: Vec::new(),
            ..DspSettings::default()
        };
        let (eq, cf) = compute_curves(&dsp, 48_000);
        assert_eq!(eq.effective_preamp_db, -6.0);
        assert!(cf.mid_db.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn curves_have_matching_lengths() {
        let (eq, cf) = compute_curves(&DspSettings::default(), DEFAULT_SAMPLE_RATE);
        assert_eq!(eq.freq_hz.len(), cf.freq_hz.len());
        assert_eq!(eq.composite_db.len(), 256);
        assert_eq!(eq.per_band_db.len(), crate::peq::GEQ_BAND_COUNT);
    }
}
