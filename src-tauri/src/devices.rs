#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

//! Audio device discovery and the Linux virtual loopback sink.
//!
//! Device `id` values are the exact strings CamillaDSP expects in its
//! `device:` field, so a listed device can be passed straight back through
//! the settings. Backend names are the canonical `Pulse`, `Alsa` and `Wasapi`
//! strings the UI uses.

use serde::{Deserialize, Serialize};

pub const LINUX_VIRTUAL_SINK_NAME: &str = "headphone_dsp_sink";
pub const LINUX_VIRTUAL_SINK_MONITOR: &str = "headphone_dsp_sink.monitor";
const LINUX_VIRTUAL_SINK_DESCRIPTION: &str = "Headphone_DSP_Virtual_Input";
const VIRTUAL_SINK_LABEL: &str = "[Virtual Sink Loopback] Headphone DSP Virtual Input";

pub const BACKEND_PULSE: &str = "Pulse";
pub const BACKEND_ALSA: &str = "Alsa";
pub const BACKEND_WASAPI: &str = "Wasapi";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub label: String,
    /// "Pulse", "Alsa" or "Wasapi" (canonical spelling).
    pub backend: String,
    /// WASAPI render endpoint captured via loopback, or a PulseAudio monitor
    /// source. Only WASAPI loopback needs a flag in the CamillaDSP config.
    pub is_loopback: bool,
    pub channels: u16,
}

/// Map any case of a backend name to its canonical spelling.
pub fn canonical_backend(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "pulse" | "pulseaudio" | "pipewire" => Some(BACKEND_PULSE),
        "alsa" => Some(BACKEND_ALSA),
        "wasapi" => Some(BACKEND_WASAPI),
        _ => None,
    }
}

/// Result of a device scan. `capture`/`playback` feed the UI's selectors.
#[derive(Debug, Clone, Default)]
pub struct DeviceList {
    pub capture: Vec<AudioDeviceInfo>,
    pub playback: Vec<AudioDeviceInfo>,
    /// Monitor source of the Linux virtual sink, when available.
    pub virtual_sink: Option<String>,
    /// Non-fatal enumeration problems (e.g. pactl missing). Logged, not fatal.
    pub warnings: Vec<String>,
}

/// Enumerate capture and playback devices for the current platform.
/// Blocking: spawns helper processes on Linux.
pub fn enumerate_devices() -> DeviceList {
    let mut list = DeviceList::default();
    #[cfg(target_os = "windows")]
    enumerate_windows(&mut list);
    #[cfg(target_os = "linux")]
    enumerate_linux(&mut list);
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    list.warnings
        .push("device enumeration is not implemented for this platform".to_string());
    for warning in &list.warnings {
        eprintln!("headphone-dsp: device scan: {warning}");
    }
    list
}

#[cfg(target_os = "windows")]
fn enumerate_windows(list: &mut DeviceList) {
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::default_host();

    match host.output_devices() {
        Ok(outputs) => {
            for device in outputs {
                let Ok(name) = device.name() else { continue };
                let channels = device
                    .default_output_config()
                    .map(|c| c.channels())
                    .unwrap_or(2);
                // Zero-driver WASAPI loopback: capture a render endpoint.
                list.capture.push(AudioDeviceInfo {
                    id: name.clone(),
                    label: format!("[WASAPI Loopback] {name}"),
                    backend: BACKEND_WASAPI.to_string(),
                    is_loopback: true,
                    channels,
                });
                list.playback.push(AudioDeviceInfo {
                    id: name.clone(),
                    label: name,
                    backend: BACKEND_WASAPI.to_string(),
                    is_loopback: false,
                    channels,
                });
            }
        }
        Err(e) => list.warnings.push(format!("WASAPI output enumeration failed: {e}")),
    }

    match host.input_devices() {
        Ok(inputs) => {
            for device in inputs {
                let Ok(name) = device.name() else { continue };
                let channels = device
                    .default_input_config()
                    .map(|c| c.channels())
                    .unwrap_or(2);
                list.capture.push(AudioDeviceInfo {
                    id: name.clone(),
                    label: name,
                    backend: BACKEND_WASAPI.to_string(),
                    is_loopback: false,
                    channels,
                });
            }
        }
        Err(e) => list.warnings.push(format!("WASAPI input enumeration failed: {e}")),
    }
}

/// Run a command and return its stdout lines. `LC_ALL=C` keeps tool output
/// (pactl's column text, sample-spec labels) locale-independent.
#[cfg(target_os = "linux")]
fn run_lines(program: &str, args: &[&str]) -> Result<Vec<String>, String> {
    let output = std::process::Command::new(program)
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .map_err(|e| format!("{program} not available: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect())
}

/// Second tab-separated column of a `pactl list short` line (the name).
fn pactl_short_name(line: &str) -> Option<&str> {
    line.split('\t').nth(1).map(str::trim).filter(|s| !s.is_empty())
}

/// Lowest and highest sample rates accepted from the platform.
const SAMPLE_RATE_RANGE: std::ops::RangeInclusive<u32> = 8_000..=384_000;

/// Sample rate from a `pactl` line (e.g. `float32le 2ch 48000Hz`): the first
/// whitespace-separated token ending in `Hz`/`hz` whose numeric prefix is a
/// plausible rate.
pub fn parse_pactl_sample_rate(line: &str) -> Option<u32> {
    line.split_whitespace().find_map(|token| {
        let number = token
            .strip_suffix("Hz")
            .or_else(|| token.strip_suffix("hz"))?;
        let rate: u32 = number.parse().ok()?;
        SAMPLE_RATE_RANGE.contains(&rate).then_some(rate)
    })
}

/// Sample rate of the audio that a capture device will deliver, from
/// `pactl list short` output.
///
/// For a `<sink>.monitor` source the sink's own rate is only a fallback: an
/// active sink-input (a playing stream routed into the sink) sets the rate
/// the sink actually carries, so the highest active input rate wins. Without
/// one, the monitor source's rate, then the sink's rate, is used. Other
/// capture devices report their own source rate.
pub fn parse_pactl_source_sample_rate(
    capture_device: &str,
    sources: &[String],
    sinks: &[String],
    sink_inputs: &[String],
) -> Option<u32> {
    let find_source_rate = || {
        sources
            .iter()
            .find(|line| pactl_short_name(line) == Some(capture_device))
            .and_then(|line| parse_pactl_sample_rate(line))
    };

    let Some(sink_name) = capture_device.strip_suffix(".monitor") else {
        return find_source_rate();
    };

    let sink_line = sinks
        .iter()
        .find(|line| pactl_short_name(line) == Some(sink_name));
    let sink_index = sink_line
        .and_then(|line| line.split('\t').next())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let sink_rate = sink_line.and_then(|line| parse_pactl_sample_rate(line));

    if let Some(index) = sink_index {
        let input_rate = sink_inputs
            .iter()
            .filter(|line| line.split('\t').nth(1).map(str::trim) == Some(index))
            .filter_map(|line| parse_pactl_sample_rate(line))
            .max();
        if input_rate.is_some() {
            return input_rate;
        }
    }

    find_source_rate().or(sink_rate)
}

/// Detect the sample rate of the active audio source for the given capture
/// and playback devices. Returns `None` when it cannot be determined.
pub fn detect_source_sample_rate(capture_device: &str, playback_device: &str) -> Option<u32> {
    detect_platform_sample_rate(capture_device, playback_device)
        .filter(|rate| SAMPLE_RATE_RANGE.contains(rate))
}

#[cfg(target_os = "linux")]
fn detect_platform_sample_rate(capture_device: &str, _playback_device: &str) -> Option<u32> {
    let sources = run_lines("pactl", &["list", "short", "sources"]).unwrap_or_default();
    let sinks = run_lines("pactl", &["list", "short", "sinks"]).unwrap_or_default();
    let sink_inputs = run_lines("pactl", &["list", "short", "sink-inputs"]).unwrap_or_default();
    parse_pactl_source_sample_rate(capture_device, &sources, &sinks, &sink_inputs).or_else(|| {
        run_lines("pactl", &["info"])
            .unwrap_or_default()
            .iter()
            .filter(|line| line.starts_with("Default Sample Specification:"))
            .find_map(|line| parse_pactl_sample_rate(line))
    })
}

#[cfg(target_os = "windows")]
fn detect_platform_sample_rate(capture_device: &str, playback_device: &str) -> Option<u32> {
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::default_host();
    let output_named = |wanted: &str| {
        host.output_devices()
            .into_iter()
            .flatten()
            .find(|device| device.name().ok().as_deref() == Some(wanted))
    };
    let input_named = |wanted: &str| {
        host.input_devices()
            .into_iter()
            .flatten()
            .find(|device| device.name().ok().as_deref() == Some(wanted))
    };

    // Capture endpoint first: a render endpoint used for loopback, then a
    // real capture device. The playback endpoint is the last resort.
    output_named(capture_device)
        .and_then(|device| device.default_output_config().ok())
        .map(|config| config.sample_rate().0)
        .or_else(|| {
            input_named(capture_device)
                .and_then(|device| device.default_input_config().ok())
                .map(|config| config.sample_rate().0)
        })
        .or_else(|| {
            output_named(playback_device)
                .and_then(|device| device.default_output_config().ok())
                .map(|config| config.sample_rate().0)
        })
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn detect_platform_sample_rate(_capture_device: &str, _playback_device: &str) -> Option<u32> {
    None
}

/// Read the current volume percentage (`0..=150`) of `headphone_dsp_sink`.
#[cfg(target_os = "linux")]
pub fn read_linux_virtual_sink_volume() -> Option<u8> {
    let lines = run_lines("pactl", &["get-sink-volume", LINUX_VIRTUAL_SINK_NAME]).ok()?;
    for line in lines {
        for part in line.split('/') {
            let trimmed = part.trim();
            if let Some(num) = trimmed.strip_suffix('%') {
                if let Ok(pct) = num.trim().parse::<u16>() {
                    return Some(pct.min(150) as u8);
                }
            }
        }
    }
    None
}

/// Apply a saved volume percentage to `headphone_dsp_sink`.
#[cfg(target_os = "linux")]
pub fn set_linux_virtual_sink_volume(percent: u8) {
    let pct = percent.clamp(0, 150);
    let _ = run_lines("pactl", &["set-sink-mute", LINUX_VIRTUAL_SINK_NAME, "0"]);
    let _ = run_lines(
        "pactl",
        &["set-sink-volume", LINUX_VIRTUAL_SINK_NAME, &format!("{pct}%")],
    );
}

/// Ensure downstream hardware/sink mixers are pinned to unity (`100%` / `0 dB`)
/// so `headphone_dsp_sink` (controlled by the desktop volume keys/slider) is
/// the single volume control stage with zero hidden attenuation downstream.
#[cfg(target_os = "linux")]
pub fn maximize_linux_output_gains(playback_backend: &str, playback_device: &str) {
    // 1. Hardware ALSA mixers on all sound cards (0..4): Master, Headphone, PCM, Front, DAC
    for card in 0..4 {
        for ctl in ["Master", "Headphone", "PCM", "Front", "DAC"] {
            let _ = std::process::Command::new("amixer")
                .args(["-c", &card.to_string(), "sset", ctl, "100%", "unmute"])
                .output();
        }
    }
    // 2. If outputting via a Pulse/PipeWire sink (not our virtual input sink),
    // set that downstream sink to 100% unmuted so it doesn't double-attenuate.
    if playback_backend.eq_ignore_ascii_case("pulse")
        && !playback_device.trim().is_empty()
        && playback_device.trim() != LINUX_VIRTUAL_SINK_NAME
    {
        let dev = playback_device.trim();
        let _ = run_lines("pactl", &["set-sink-mute", dev, "0"]);
        let _ = run_lines("pactl", &["set-sink-volume", dev, "100%"]);
    }
}

/// Make sure the virtual null sink exists and return its monitor source.
///
/// Loads `module-null-sink` if `headphone_dsp_sink` is missing, so any
/// PipeWire/PulseAudio desktop gets a capture source with zero setup. Modules
/// do not persist across reboots, so this runs on every device refresh.
#[cfg(target_os = "linux")]
pub fn ensure_linux_virtual_sink() -> Result<String, String> {
    maximize_linux_output_gains("", "");

    let sinks = run_lines("pactl", &["list", "short", "sinks"])?;
    let exists = sinks
        .iter()
        .any(|line| pactl_short_name(line) == Some(LINUX_VIRTUAL_SINK_NAME));
    if !exists {
        let sink_name_arg = format!("sink_name={LINUX_VIRTUAL_SINK_NAME}");
        let sink_properties_arg = format!("sink_properties=device.description={LINUX_VIRTUAL_SINK_DESCRIPTION}");
        run_lines(
            "pactl",
            &["load-module", "module-null-sink", &sink_name_arg, &sink_properties_arg],
        )?;
        let _ = run_lines("pactl", &["set-sink-mute", LINUX_VIRTUAL_SINK_NAME, "0"]);
    }
    Ok(LINUX_VIRTUAL_SINK_MONITOR.to_string())
}

/// A PipeWire node as seen by `pw-dump`.
#[derive(Debug, Clone, PartialEq)]
pub struct PwNode {
    pub name: String,
    pub description: Option<String>,
    /// "Audio/Sink" or "Audio/Source".
    pub media_class: String,
}

/// Parse `pw-dump` JSON into audio sink and source nodes.
pub fn parse_pw_dump(json: &str) -> Result<Vec<PwNode>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("pw-dump output is not JSON: {e}"))?;
    let objects = value.as_array().ok_or("pw-dump output is not an array")?;
    let mut nodes = Vec::new();
    for object in objects {
        let is_node = object
            .get("type")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.ends_with(":Node"));
        if !is_node {
            continue;
        }
        let props = match object.pointer("/info/props") {
            Some(p) => p,
            None => continue,
        };
        let media_class = props.get("media.class").and_then(|v| v.as_str()).unwrap_or_default();
        if media_class != "Audio/Sink" && media_class != "Audio/Source" {
            continue;
        }
        let Some(name) = props.get("node.name").and_then(|v| v.as_str()) else { continue };
        nodes.push(PwNode {
            name: name.to_string(),
            description: props
                .get("node.description")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            media_class: media_class.to_string(),
        });
    }
    Ok(nodes)
}

fn pulse_label(name: &str, descriptions: &[(String, String)]) -> String {
    match descriptions.iter().find(|(n, _)| n == name) {
        Some((_, description)) => format!("{description} ({name})"),
        None => name.to_string(),
    }
}

/// Build PulseAudio/PipeWire capture and playback entries from
/// `pactl list short sources|sinks` output. Monitor sources are loopbacks.
pub fn parse_pactl_devices(
    sources: &[String],
    sinks: &[String],
    descriptions: &[(String, String)],
) -> (Vec<AudioDeviceInfo>, Vec<AudioDeviceInfo>) {
    let mut capture = Vec::new();
    let mut playback = Vec::new();
    for line in sources {
        let Some(name) = pactl_short_name(line) else { continue };
        let is_monitor = name.ends_with(".monitor");
        let label = if name == LINUX_VIRTUAL_SINK_MONITOR {
            VIRTUAL_SINK_LABEL.to_string()
        } else if is_monitor {
            format!("[Monitor] {}", pulse_label(name, descriptions))
        } else {
            pulse_label(name, descriptions)
        };
        capture.push(AudioDeviceInfo {
            id: name.to_string(),
            label,
            backend: BACKEND_PULSE.to_string(),
            is_loopback: is_monitor,
            channels: 2,
        });
    }
    for line in sinks {
        let Some(name) = pactl_short_name(line) else { continue };
        if name == LINUX_VIRTUAL_SINK_NAME {
            continue;
        }
        playback.push(AudioDeviceInfo {
            id: name.to_string(),
            label: pulse_label(name, descriptions),
            backend: BACKEND_PULSE.to_string(),
            is_loopback: false,
            channels: 2,
        });
    }
    (capture, playback)
}

/// Build the same entries from `pw-dump` nodes, for PipeWire systems without
/// `pactl`. Each sink also yields a `<name>.monitor` loopback source.
pub fn pw_nodes_to_devices(nodes: &[PwNode]) -> (Vec<AudioDeviceInfo>, Vec<AudioDeviceInfo>) {
    let mut capture = Vec::new();
    let mut playback = Vec::new();
    for node in nodes {
        let label = node.description.clone().unwrap_or_else(|| node.name.clone());
        if node.media_class == "Audio/Sink" {
            playback.push(AudioDeviceInfo {
                id: node.name.clone(),
                label: label.clone(),
                backend: BACKEND_PULSE.to_string(),
                is_loopback: false,
                channels: 2,
            });
            let monitor = format!("{}.monitor", node.name);
            let monitor_label = if monitor == LINUX_VIRTUAL_SINK_MONITOR {
                VIRTUAL_SINK_LABEL.to_string()
            } else {
                format!("[Monitor] {label}")
            };
            capture.push(AudioDeviceInfo {
                id: monitor,
                label: monitor_label,
                backend: BACKEND_PULSE.to_string(),
                is_loopback: true,
                channels: 2,
            });
        } else {
            capture.push(AudioDeviceInfo {
                id: node.name.clone(),
                label,
                backend: BACKEND_PULSE.to_string(),
                is_loopback: false,
                channels: 2,
            });
        }
    }
    (capture, playback)
}

/// Parse `aplay -L` / `arecord -L` output into hardware ALSA devices.
///
/// Names start at column 0 and their description is indented. Only
/// `hw:` and `plughw:` addresses are kept. `plughw:` entries are ordered
/// first: the ALSA plug layer converts sample format and rate, so CamillaDSP
/// does not fail when the hardware refuses the exact format it asked for.
pub fn parse_alsa_list(text: &str) -> Vec<AudioDeviceInfo> {
    let mut devices = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            let name = line.trim().to_string();
            current = if name.starts_with("hw:") || name.starts_with("plughw:") {
                Some(name)
            } else {
                None
            };
        } else if let Some(name) = current.take() {
            let description = line.trim();
            let label = if description.is_empty() {
                format!("ALSA {name}")
            } else {
                format!("{description} ({name})")
            };
            devices.push(AudioDeviceInfo {
                id: name,
                label,
                backend: BACKEND_ALSA.to_string(),
                is_loopback: false,
                channels: 2,
            });
        }
    }
    // Stable sort: plughw: entries first, original order otherwise.
    devices.sort_by_key(|d| !d.id.starts_with("plughw:"));
    devices
}

#[cfg(target_os = "linux")]
fn alsa_devices(tool: &str) -> Vec<AudioDeviceInfo> {
    match std::process::Command::new(tool).arg("-L").output() {
        Ok(o) if o.status.success() => {
            parse_alsa_list(&String::from_utf8_lossy(&o.stdout))
        }
        _ => Vec::new(),
    }
}

#[cfg(target_os = "linux")]
fn enumerate_linux(list: &mut DeviceList) {
    match ensure_linux_virtual_sink() {
        Ok(monitor) => list.virtual_sink = Some(monitor),
        Err(e) => list.warnings.push(format!("virtual sink unavailable: {e}")),
    }

    // pw-dump gives human-readable descriptions when it is present and is the
    // fallback source of names on PipeWire systems without pactl.
    let pw_nodes = run_lines("pw-dump", &[])
        .and_then(|lines| parse_pw_dump(&lines.join("\n")));

    let pactl = (
        run_lines("pactl", &["list", "short", "sources"]),
        run_lines("pactl", &["list", "short", "sinks"]),
    );
    match pactl {
        (Ok(sources), Ok(sinks)) => {
            let descriptions: Vec<(String, String)> = pw_nodes
                .as_ref()
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter_map(|n| n.description.clone().map(|d| (n.name.clone(), d)))
                        .collect()
                })
                .unwrap_or_default();
            let (capture, playback) = parse_pactl_devices(&sources, &sinks, &descriptions);
            list.capture.extend(capture);
            list.playback.extend(playback);
        }
        (sources, sinks) => {
            let reason = sources.err().or(sinks.err()).unwrap_or_default();
            match pw_nodes {
                Ok(nodes) => {
                    let (capture, playback) = pw_nodes_to_devices(&nodes);
                    list.capture.extend(capture);
                    list.playback.extend(playback);
                }
                Err(e) => list.warnings.push(format!("no PulseAudio/PipeWire listing ({reason}); {e}")),
            }
        }
    }

    // Hardware ALSA playback devices (CamillaDSP `type: Alsa`).
    list.playback.extend(alsa_devices("aplay"));

    // When the dedicated virtual loopback sink is available on Linux, it is the
    // single authoritative capture source — omit microphones and hardware
    // monitor sources so the UI does not need a capture dropdown at all.
    if list.virtual_sink.is_some() {
        list.capture.retain(|d| d.id == LINUX_VIRTUAL_SINK_MONITOR);
        if list.capture.is_empty() {
            list.capture.push(AudioDeviceInfo {
                id: LINUX_VIRTUAL_SINK_MONITOR.to_string(),
                label: VIRTUAL_SINK_LABEL.to_string(),
                backend: BACKEND_PULSE.to_string(),
                is_loopback: true,
                channels: 2,
            });
        }
    } else {
        list.capture.extend(alsa_devices("arecord"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_info_round_trips_through_json() {
        let info = AudioDeviceInfo {
            id: "headphone_dsp_sink.monitor".into(),
            label: VIRTUAL_SINK_LABEL.into(),
            backend: "Pulse".into(),
            is_loopback: true,
            channels: 2,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: AudioDeviceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn virtual_sink_constants_are_consistent() {
        assert_eq!(LINUX_VIRTUAL_SINK_MONITOR, format!("{LINUX_VIRTUAL_SINK_NAME}.monitor"));
    }

    #[test]
    fn backend_names_are_canonicalized_case_insensitively() {
        assert_eq!(canonical_backend("pulse"), Some("Pulse"));
        assert_eq!(canonical_backend("ALSA"), Some("Alsa"));
        assert_eq!(canonical_backend("wAsApI"), Some("Wasapi"));
        assert_eq!(canonical_backend("jack"), None);
    }

    #[test]
    fn alsa_list_puts_plughw_first_and_keeps_descriptions() {
        let text = "\
null
    Discard all samples (playback) or generate zero samples (capture)
hw:CARD=Generic,DEV=3
    HD-Audio Generic, HDMI 0
    Direct hardware device without any conversions
plughw:CARD=Generic_1,DEV=0
    ALC257 Analog
    Hardware device with all software conversions
plughw:CARD=Generic,DEV=3
    HD-Audio Generic, HDMI 0
default
    Default ALSA Output
";
        let devices = parse_alsa_list(text);
        let ids: Vec<&str> = devices.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec![
            "plughw:CARD=Generic_1,DEV=0",
            "plughw:CARD=Generic,DEV=3",
            "hw:CARD=Generic,DEV=3",
        ]);
        assert_eq!(devices[0].label, "ALC257 Analog (plughw:CARD=Generic_1,DEV=0)");
        assert!(devices.iter().all(|d| d.backend == "Alsa" && !d.is_loopback));
    }

    #[test]
    fn pactl_parsing_marks_monitors_and_hides_virtual_sink_playback() {
        let sources = vec![
            "0\talsa_input.pci\tmodule\ts16le\tIDLE".to_string(),
            "1\theadphone_dsp_sink.monitor\tmodule\ts16le\tIDLE".to_string(),
            "2\talsa_output.pci.monitor\tmodule\ts16le\tIDLE".to_string(),
        ];
        let sinks = vec![
            "0\talsa_output.pci\tmodule\ts16le\tIDLE".to_string(),
            "1\theadphone_dsp_sink\tmodule\ts16le\tIDLE".to_string(),
        ];
        let descriptions = vec![("alsa_output.pci".to_string(), "Built-in Audio".to_string())];
        let (capture, playback) = parse_pactl_devices(&sources, &sinks, &descriptions);
        assert_eq!(capture.len(), 3);
        assert!(!capture[0].is_loopback);
        assert!(capture[1].is_loopback);
        assert_eq!(capture[1].label, VIRTUAL_SINK_LABEL);
        assert!(capture[2].is_loopback);
        assert_eq!(playback.len(), 1, "virtual sink is not offered as an output");
        assert_eq!(playback[0].id, "alsa_output.pci");
    }

    #[test]
    fn pactl_sample_rate_is_read_from_the_spec_token() {
        assert_eq!(
            parse_pactl_sample_rate("874\theadphone_dsp_sink.monitor\tPipeWire\tfloat32le 2ch 48000Hz\tSUSPENDED"),
            Some(48_000)
        );
        assert_eq!(parse_pactl_sample_rate("Default Sample Specification: s16le 2ch 44100hz"), Some(44_100));
        assert_eq!(parse_pactl_sample_rate("0\tsink\tmod\ts16le 2ch 7999Hz\tIDLE"), None, "below range");
        assert_eq!(parse_pactl_sample_rate("0\tsink\tmod\ts16le 2ch 384001Hz\tIDLE"), None, "above range");
        assert_eq!(parse_pactl_sample_rate("0\tsink\tmod\ts16le 2ch 48000\tIDLE"), None, "no Hz suffix");
        assert_eq!(parse_pactl_sample_rate("0\tsink\tmod\ts16le 2ch xHz\tIDLE"), None, "not numeric");
    }

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn monitor_rate_follows_active_sink_input_over_idle_virtual_sink() {
        let sources = lines(&["1\theadphone_dsp_sink.monitor\tmodule\ts16le 2ch 48000Hz\tIDLE"]);
        let sinks = lines(&["1\theadphone_dsp_sink\tmodule\tfloat32le 2ch 48000Hz\tIDLE"]);
        let at_44k = lines(&["7\t1\tspotify\tprotocol\tfloat32le 2ch 44100Hz\tRUNNING"]);
        assert_eq!(
            parse_pactl_source_sample_rate("headphone_dsp_sink.monitor", &sources, &sinks, &at_44k),
            Some(44_100)
        );
        let at_96k = lines(&["7\t1\tplayer\tprotocol\tfloat32le 2ch 96000Hz\tRUNNING"]);
        assert_eq!(
            parse_pactl_source_sample_rate("headphone_dsp_sink.monitor", &sources, &sinks, &at_96k),
            Some(96_000)
        );
    }

    #[test]
    fn monitor_rate_falls_back_to_source_then_sink_without_active_inputs() {
        let sources = lines(&["1\theadphone_dsp_sink.monitor\tmodule\ts16le 2ch 48000Hz\tIDLE"]);
        let sinks = lines(&["1\theadphone_dsp_sink\tmodule\tfloat32le 2ch 44100Hz\tIDLE"]);
        assert_eq!(
            parse_pactl_source_sample_rate("headphone_dsp_sink.monitor", &sources, &sinks, &[]),
            Some(48_000),
            "monitor source rate when no sink-input is active"
        );
        assert_eq!(
            parse_pactl_source_sample_rate("headphone_dsp_sink.monitor", &[], &sinks, &[]),
            Some(44_100),
            "sink rate when the monitor source is missing"
        );
        assert_eq!(parse_pactl_source_sample_rate("headphone_dsp_sink.monitor", &[], &[], &[]), None);
    }

    #[test]
    fn sink_inputs_on_other_sinks_are_ignored() {
        let sources = lines(&["1\theadphone_dsp_sink.monitor\tmodule\ts16le 2ch 48000Hz\tIDLE"]);
        let sinks = lines(&[
            "1\theadphone_dsp_sink\tmodule\tfloat32le 2ch 48000Hz\tIDLE",
            "2\talsa_output.pci\tmodule\ts32le 2ch 192000Hz\tRUNNING",
        ]);
        let sink_inputs = lines(&["9\t2\tbrowser\tprotocol\tfloat32le 2ch 192000Hz\tRUNNING"]);
        assert_eq!(
            parse_pactl_source_sample_rate("headphone_dsp_sink.monitor", &sources, &sinks, &sink_inputs),
            Some(48_000),
            "a stream on another sink must not drive this sink's rate"
        );
    }

    #[test]
    fn non_monitor_capture_uses_its_own_source_rate() {
        let sources = lines(&["3\talsa_input.usb\tmodule\ts24le 2ch 96000Hz\tRUNNING"]);
        let sinks = lines(&["3\talsa_input.usb\tmodule\ts24le 2ch 44100Hz\tIDLE"]);
        assert_eq!(parse_pactl_source_sample_rate("alsa_input.usb", &sources, &sinks, &[]), Some(96_000));
        assert_eq!(parse_pactl_source_sample_rate("alsa_input.missing", &sources, &sinks, &[]), None);
    }

    #[test]
    fn pw_dump_parsing_extracts_sinks_and_sources() {
        let json = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client", "info": {"props": {}}},
          {"id": 2, "type": "PipeWire:Interface:Node", "info": {"props": {
              "media.class": "Audio/Sink", "node.name": "alsa_output.pci",
              "node.description": "Built-in Audio"}}},
          {"id": 3, "type": "PipeWire:Interface:Node", "info": {"props": {
              "media.class": "Audio/Source", "node.name": "alsa_input.pci"}}},
          {"id": 4, "type": "PipeWire:Interface:Node", "info": {"props": {
              "media.class": "Stream/Output/Audio", "node.name": "firefox"}}}
        ]"#;
        let nodes = parse_pw_dump(json).unwrap();
        assert_eq!(nodes.len(), 2);
        let (capture, playback) = pw_nodes_to_devices(&nodes);
        assert_eq!(playback[0].label, "Built-in Audio");
        assert_eq!(capture[0].id, "alsa_output.pci.monitor");
        assert!(capture[0].is_loopback);
        assert_eq!(capture[1].id, "alsa_input.pci");
        assert!(parse_pw_dump("not json").is_err());
    }
}
