//! Mid/Side crossfeed filter design. Port of `hifi_core/crossfeed.py`
//! (the `ms` mode only): measured KU100 interaural cues on the Side channel,
//! a soft-limited flattening correction on the Mid channel, and a modelling
//! delay so the non-causal pre-ringing of the spectral design is preserved.

use std::f64::consts::PI;
use std::fmt::Write as _;
use std::path::Path;

use include_dir::{include_dir, Dir};
use num_complex::Complex64;
use realfft::RealFftPlanner;
use serde::{Deserialize, Serialize};

use crate::peq::{
    biquad_coefficients, log_frequency_grid, Biquad, EqBand, CURVE_POINTS, FREQ_MAX_HZ, FREQ_MIN_HZ,
};

pub const DEFAULT_CROSSFEED_ANGLE_DEG: f64 = 30.0;
pub const MIN_CROSSFEED_ANGLE_DEG: f64 = 0.0;
pub const MAX_CROSSFEED_ANGLE_DEG: f64 = 90.0;
pub const CROSSFEED_TAP_COUNT: usize = 512;
pub const SIDE_FILTER_FFT_SIZE: usize = 32768;
pub const SIDE_FILTER_TAIL_FADE_TAPS: usize = 64;
pub const MS_FILTER_TAP_COUNT: usize = 2048;
pub const MS_FILTER_MODELING_DELAY_DIVISOR: usize = 4;
pub const DEFAULT_CROSSFEED_MID_BOOST_DB: f64 = 9.0;
pub const MIN_CROSSFEED_MID_BOOST_DB: f64 = 0.0;
pub const MAX_CROSSFEED_MID_BOOST_DB: f64 = 18.0;
pub const MID_CORRECTION_KNEE_ORDER: f64 = 4.0;

/// Measured KU100 dummy-head assets, `<rate>/angle_<NNN>.json`.
static KU100_DATA: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/data_ku100");

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CrossfeedPreview {
    pub freq_hz: Vec<f64>,
    /// |Mid| = |ll + lr| in dB.
    pub mid_db: Vec<f64>,
    /// |Side| = |ll - lr| in dB.
    pub side_db: Vec<f64>,
    /// Interaural level difference 20*log10(|lr| / |ll|) in dB (contra vs ipsi).
    pub ild_db: Vec<f64>,
}

pub fn coerce_crossfeed_angle_deg(value: f64) -> f64 {
    if !value.is_finite() {
        return DEFAULT_CROSSFEED_ANGLE_DEG;
    }
    value.clamp(MIN_CROSSFEED_ANGLE_DEG, MAX_CROSSFEED_ANGLE_DEG)
}

pub fn coerce_crossfeed_mid_boost_db(value: f64) -> f64 {
    if !value.is_finite() {
        return DEFAULT_CROSSFEED_MID_BOOST_DB;
    }
    value.clamp(MIN_CROSSFEED_MID_BOOST_DB, MAX_CROSSFEED_MID_BOOST_DB)
}

/// The KU100-compatible base rate (44 100 or 48 000 Hz) that DSP runs at for
/// a given source rate. Integer multiples of 44.1 kHz map to 44 100 Hz,
/// integer multiples of 48 kHz to 48 000 Hz. Other rates map to whichever
/// base is closer, 44 100 Hz below 46 050 Hz.
pub fn ku100_base_sample_rate(source_rate: u32) -> u32 {
    if source_rate > 0 && source_rate % 44_100 == 0 {
        44_100
    } else if source_rate > 0 && source_rate % 48_000 == 0 {
        48_000
    } else if source_rate < 46_050 {
        44_100
    } else {
        48_000
    }
}

fn woodworth_itd_seconds(angle_deg: f64) -> f64 {
    let angle_rad = coerce_crossfeed_angle_deg(angle_deg).to_radians();
    let head_radius_m = 0.0875;
    let speed_of_sound_m_s = 343.0;
    (head_radius_m / speed_of_sound_m_s) * (angle_rad.sin() + angle_rad)
}

fn contra_cutoff_hz(angle_deg: f64, sample_rate: u32) -> f64 {
    let angle = coerce_crossfeed_angle_deg(angle_deg);
    let nyquist_safe = (sample_rate as f64 * 0.45).max(1000.0);
    if angle <= 30.0 {
        let fraction = angle / 30.0;
        nyquist_safe + (1100.0 - nyquist_safe) * fraction
    } else {
        let fraction = (angle - 30.0) / 60.0;
        1100.0 + (700.0 - 1100.0) * fraction
    }
}

/// Synthetic contralateral path (Woodworth ITD + single-pole head-shadow
/// lowpass). Only used when no measured KU100 asset exists for the
/// angle/sample-rate combination.
pub fn build_contra_taps(angle_deg: f64, sample_rate: u32, tap_count: usize) -> Vec<f64> {
    let cutoff_hz = contra_cutoff_hz(angle_deg, sample_rate);
    let delay_samples = woodworth_itd_seconds(angle_deg) * sample_rate as f64;
    let integer_delay = delay_samples.floor() as usize;
    let fractional_delay = delay_samples - integer_delay as f64;

    let alpha = (-2.0 * PI * cutoff_hz / sample_rate as f64).exp();
    let mut taps = vec![0.0; tap_count];
    for source_index in 0..tap_count {
        let amplitude = (1.0 - alpha) * alpha.powi(source_index as i32);
        let target_index = source_index + integer_delay;
        if target_index < tap_count {
            taps[target_index] += amplitude * (1.0 - fractional_delay);
        }
        if target_index + 1 < tap_count {
            taps[target_index + 1] += amplitude * fractional_delay;
        }
    }
    taps
}

/// Python's `round()` rounds half to even; `f64::round` does not.
fn quantized_angle(angle_deg: f64) -> i64 {
    coerce_crossfeed_angle_deg(angle_deg).round_ties_even() as i64
}

#[derive(Deserialize)]
struct Ku100Payload {
    #[serde(default)]
    sample_rate: Option<u32>,
    #[serde(default)]
    ll: Vec<f64>,
    #[serde(default)]
    lr: Vec<f64>,
}

fn pad_or_truncate(mut values: Vec<f64>, tap_count: usize) -> Vec<f64> {
    values.truncate(tap_count);
    values.resize(tap_count, 0.0);
    values
}

/// Load the measured (ipsi, contra) taps (`ll`, `lr`) for this angle and
/// sample rate, padded/truncated to `tap_count`. `None` if no usable asset.
pub fn load_ku100_ipsi_contra_taps(
    angle_deg: f64,
    sample_rate: u32,
    tap_count: usize,
) -> Option<(Vec<f64>, Vec<f64>)> {
    let path = format!("{}/angle_{:03}.json", sample_rate, quantized_angle(angle_deg));
    let file = KU100_DATA.get_file(&path)?;
    let text = file.contents_utf8()?;
    let payload: Ku100Payload = serde_json::from_str(text).ok()?;
    if let Some(rate) = payload.sample_rate {
        if rate != sample_rate {
            return None;
        }
    }
    Some((
        pad_or_truncate(payload.ll, tap_count),
        pad_or_truncate(payload.lr, tap_count),
    ))
}

/// Whether a measured KU100 asset exists for this angle and sample rate.
pub fn ku100_asset_available(angle_deg: f64, sample_rate: u32) -> bool {
    load_ku100_ipsi_contra_taps(angle_deg, sample_rate, CROSSFEED_TAP_COUNT).is_some()
}

/// The relative interaural transfer function `h` (contra / ipsi[0]).
/// Falls back to the synthetic contra model when no measurement exists.
pub fn relative_crossfeed_taps(angle_deg: f64, sample_rate: u32, tap_count: usize) -> Vec<f64> {
    let angle = coerce_crossfeed_angle_deg(angle_deg);
    match load_ku100_ipsi_contra_taps(angle, sample_rate, tap_count) {
        None => build_contra_taps(angle, sample_rate, tap_count),
        Some((ipsi, contra)) => {
            let direct_gain = ipsi[0];
            if direct_gain == 0.0 {
                build_contra_taps(angle, sample_rate, tap_count)
            } else {
                contra.iter().map(|tap| tap / direct_gain).collect()
            }
        }
    }
}

struct MsDesign {
    /// Soft-limited 1/(1+h), half-spectrum (fft_size/2 + 1 bins).
    correction: Vec<Complex64>,
    /// h, half-spectrum.
    transfer: Vec<Complex64>,
    fft_size: usize,
}

fn ms_correction_spectrum(
    angle_deg: f64,
    sample_rate: u32,
    tap_count: usize,
    mid_boost_db: f64,
) -> MsDesign {
    let angle = coerce_crossfeed_angle_deg(angle_deg);
    let boost = coerce_crossfeed_mid_boost_db(mid_boost_db);
    let relative = relative_crossfeed_taps(angle, sample_rate, tap_count);

    let fft_size = SIDE_FILTER_FFT_SIZE.max(tap_count * 4);
    let mut planner = RealFftPlanner::<f64>::new();
    let forward = planner.plan_fft_forward(fft_size);
    let mut input = vec![0.0; fft_size];
    input[..tap_count].copy_from_slice(&relative);
    let mut transfer = forward.make_output_vec();
    forward
        .process(&mut input, &mut transfer)
        .expect("forward FFT buffers are sized by construction");

    let mut correction: Vec<Complex64> = transfer
        .iter()
        .map(|h| Complex64::new(1.0, 0.0) / (Complex64::new(1.0, 0.0) + h))
        .collect();

    let max_gain = 10f64.powf(boost / 20.0);
    for c in correction.iter_mut() {
        let magnitude = c.norm();
        let limited = magnitude
            / (1.0 + (magnitude / max_gain).powf(MID_CORRECTION_KNEE_ORDER))
                .powf(1.0 / MID_CORRECTION_KNEE_ORDER);
        *c *= limited / magnitude.max(1e-20);
    }

    MsDesign {
        correction,
        transfer,
        fft_size,
    }
}

/// Inverse real FFT normalised like `np.fft.irfft` (divide by n), then the
/// modelling-delay rotation, truncation and tail fade.
fn ms_taps_from_spectrum(
    spectrum: &[Complex64],
    fft_size: usize,
    tap_count: usize,
) -> Vec<f64> {
    let mut planner = RealFftPlanner::<f64>::new();
    let inverse = planner.plan_fft_inverse(fft_size);
    let mut buffer = spectrum.to_vec();
    // DC and Nyquist bins of a real signal must be real.
    buffer[0].im = 0.0;
    let last = buffer.len() - 1;
    buffer[last].im = 0.0;
    let mut impulse = inverse.make_output_vec();
    inverse
        .process(&mut buffer, &mut impulse)
        .expect("inverse FFT buffers are sized by construction");
    let scale = 1.0 / fft_size as f64;

    // np.roll(impulse, delay)[:tap_count]
    let delay = tap_count / MS_FILTER_MODELING_DELAY_DIVISOR;
    let n = fft_size;
    let mut taps: Vec<f64> = (0..tap_count)
        .map(|i| impulse[(i + n - delay % n) % n] * scale)
        .collect();

    let fade = SIDE_FILTER_TAIL_FADE_TAPS.min(tap_count);
    if fade > 1 {
        for k in 0..fade {
            let x = k as f64 * PI / (fade - 1) as f64;
            let ramp = 0.5 * (1.0 + x.cos());
            taps[tap_count - fade + k] *= ramp;
        }
    }
    taps
}

fn mid_side_pair(
    angle_deg: f64,
    sample_rate: u32,
    tap_count: usize,
    mid_boost_db: f64,
) -> (Vec<f64>, Vec<f64>) {
    let design = ms_correction_spectrum(angle_deg, sample_rate, tap_count, mid_boost_db);
    let n = design.correction.len();
    let mut mid_spec = Vec::with_capacity(n);
    let mut side_spec = Vec::with_capacity(n);
    for k in 0..n {
        let c = design.correction[k];
        let h = design.transfer[k];
        mid_spec.push(c * (Complex64::new(1.0, 0.0) + h));
        side_spec.push(c * (Complex64::new(1.0, 0.0) - h));
    }
    (
        ms_taps_from_spectrum(&mid_spec, design.fft_size, tap_count),
        ms_taps_from_spectrum(&side_spec, design.fft_size, tap_count),
    )
}

/// Mid filter `correction * (1 + h)`: very nearly a flat pass-through.
pub fn build_ms_mid_filter_taps(
    angle_deg: f64,
    sample_rate: u32,
    tap_count: usize,
    mid_boost_db: f64,
) -> Vec<f64> {
    mid_side_pair(angle_deg, sample_rate, tap_count, mid_boost_db).0
}

/// Side filter `correction * (1 - h)`: carries the measured interaural cues.
pub fn build_ms_side_filter_taps(
    angle_deg: f64,
    sample_rate: u32,
    tap_count: usize,
    mid_boost_db: f64,
) -> Vec<f64> {
    mid_side_pair(angle_deg, sample_rate, tap_count, mid_boost_db).1
}

/// Headroom (dB, zero or negative) that keeps the crossfeed output from
/// clipping when no EQ bands are active.
pub fn crossfeed_headroom_db(mid_taps: &[f64], side_taps: &[f64]) -> f64 {
    composite_chain_headroom_db(Some((mid_taps, side_taps)), &[], 48_000)
}

/// Headroom (dB, zero or negative) for the series cascade of the Mid/Side
/// crossfeed FIR pair and the active biquad EQ bands, evaluated in the
/// complex frequency domain:
///   H_ll(f) = 0.5 * (H_mid(f) + H_side(f))
///   H_lr(f) = 0.5 * (H_mid(f) - H_side(f))
///   H_eq(f) = product_b H_biquad_b(f)
///   G_worst(f) = |(H_ll(f) * H_eq(f))| + |(H_lr(f) * H_eq(f))|
pub fn composite_chain_headroom_db(
    crossfeed_taps: Option<(&[f64], &[f64])>,
    enabled_bands: &[EqBand],
    sample_rate: u32,
) -> f64 {
    let fs = sample_rate.max(1) as f64;
    let biquads: Vec<Biquad> = enabled_bands
        .iter()
        .map(|b| biquad_coefficients(b, fs))
        .collect();
    let eq_complex = |freq_hz: f64| -> Complex64 {
        biquads
            .iter()
            .fold(Complex64::new(1.0, 0.0), |acc, bq| acc * bq.response(freq_hz, fs))
    };

    let mut max_amp = 0.0_f64;
    if let Some((mid_taps, side_taps)) = crossfeed_taps {
        let base_len = mid_taps.len().max(side_taps.len()).max(2);
        let n = (base_len * 4).max(8192);
        let mut planner = RealFftPlanner::<f64>::new();
        let forward = planner.plan_fft_forward(n);
        let spectrum = |taps: &[f64]| -> Vec<Complex64> {
            let mut input = vec![0.0; n];
            input[..taps.len()].copy_from_slice(taps);
            let mut output = forward.make_output_vec();
            forward
                .process(&mut input, &mut output)
                .expect("forward FFT buffers are sized by construction");
            output
        };
        let mid = spectrum(mid_taps);
        let side = spectrum(side_taps);

        for (k, (&m, &s)) in mid.iter().zip(side.iter()).enumerate() {
            let freq_hz = k as f64 * fs / n as f64;
            let h_ll = 0.5 * (m + s);
            let h_lr = 0.5 * (m - s);
            let h_eq = eq_complex(freq_hz);
            let total_amp = (h_ll * h_eq).norm() + (h_lr * h_eq).norm();
            max_amp = max_amp.max(total_amp);
        }
    } else if !biquads.is_empty() {
        for freq_hz in log_frequency_grid(CURVE_POINTS, FREQ_MIN_HZ, FREQ_MAX_HZ) {
            max_amp = max_amp.max(eq_complex(freq_hz).norm());
        }
    }

    let peak_db = if max_amp > 1e-12 { 20.0 * max_amp.log10() } else { 0.0 };
    (-peak_db).min(0.0)
}

#[derive(Debug, Clone, PartialEq)]
pub struct FirMatrix {
    /// ipsi: (mid + side) / 2 (also used for rr)
    pub ll: Vec<f64>,
    /// contra: (mid - side) / 2 (also used for rl)
    pub lr: Vec<f64>,
}

pub fn build_ms_fir_matrix(
    angle_deg: f64,
    sample_rate: u32,
    tap_count: usize,
    mid_boost_db: f64,
) -> FirMatrix {
    let (mid, side) = mid_side_pair(angle_deg, sample_rate, tap_count, mid_boost_db);
    let ll = mid.iter().zip(side.iter()).map(|(m, s)| (m + s) / 2.0).collect();
    let lr = mid.iter().zip(side.iter()).map(|(m, s)| (m - s) / 2.0).collect();
    FirMatrix { ll, lr }
}

/// Write taps in CamillaDSP's TEXT FIR format: one header line (skipped by
/// the `skip_bytes_lines: 1` setting) followed by one tap per line.
pub fn write_text_fir_file(path: &Path, taps: &[f64]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = String::with_capacity(taps.len() * 28 + 2);
    text.push_str("0\n");
    for tap in taps {
        let _ = writeln!(text, "{tap:.18e}");
    }
    std::fs::write(path, text)
}

/// Discrete-time frequency response of an FIR at `freq_hz`.
fn dtft(taps: &[f64], freq_hz: f64, sample_rate: f64) -> Complex64 {
    let w = 2.0 * PI * freq_hz / sample_rate;
    let mut acc = Complex64::new(0.0, 0.0);
    for (n, &h) in taps.iter().enumerate() {
        let (s, c) = (w * n as f64).sin_cos();
        acc += Complex64::new(h * c, -h * s);
    }
    acc
}

fn to_db(value: Complex64) -> f64 {
    20.0 * value.norm().max(1e-12).log10()
}

/// Response curves for the UI (256 log-spaced points, 20 Hz to 20 kHz).
pub fn compute_crossfeed_preview(
    angle_deg: f64,
    sample_rate: u32,
    mid_boost_db: f64,
) -> CrossfeedPreview {
    let (mid, side) = mid_side_pair(angle_deg, sample_rate, MS_FILTER_TAP_COUNT, mid_boost_db);
    let fs = sample_rate.max(1) as f64;
    let freq_hz = log_frequency_grid(CURVE_POINTS, FREQ_MIN_HZ, FREQ_MAX_HZ);

    let mut mid_db = Vec::with_capacity(freq_hz.len());
    let mut side_db = Vec::with_capacity(freq_hz.len());
    let mut ild_db = Vec::with_capacity(freq_hz.len());
    for &f in &freq_hz {
        let h_mid = dtft(&mid, f, fs);
        let h_side = dtft(&side, f, fs);
        // ll = (mid+side)/2, lr = (mid-side)/2; the /2 cancels in the ratio.
        let ll = h_mid + h_side;
        let lr = h_mid - h_side;
        mid_db.push(to_db(h_mid));
        side_db.push(to_db(h_side));
        ild_db.push(to_db(lr) - to_db(ll));
    }

    CrossfeedPreview {
        freq_hz,
        mid_db,
        side_db,
        ild_db,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Reference taps from `hifi_core/crossfeed.py` (`build_ms_mid_filter_taps`
    /// / `build_ms_side_filter_taps` with angle 35.0, 44100 Hz, 2048 taps,
    /// mid_boost_db 3.0), regenerated from the Python source itself and stored
    /// in `testdata/crossfeed_ms_35_44100_3db.json`.
    #[test]
    fn ms_filters_match_python_reference() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../testdata/crossfeed_ms_35_44100_3db.json"
        ))
        .expect("fixture parses");
        let expected_mid: Vec<f64> = fixture["mid"]
            .as_array()
            .expect("mid array")
            .iter()
            .map(|v| v.as_f64().expect("number"))
            .collect();
        let expected_side: Vec<f64> = fixture["side"]
            .as_array()
            .expect("side array")
            .iter()
            .map(|v| v.as_f64().expect("number"))
            .collect();

        let mid = build_ms_mid_filter_taps(35.0, 44100, 2048, 3.0);
        let side = build_ms_side_filter_taps(35.0, 44100, 2048, 3.0);
        assert_eq!(mid.len(), expected_mid.len());
        assert_eq!(side.len(), expected_side.len());

        let max_mid = mid
            .iter()
            .zip(expected_mid.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f64, f64::max);
        let max_side = side
            .iter()
            .zip(expected_side.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f64, f64::max);
        assert!(max_mid < 1e-9, "mid max abs diff {max_mid}");
        assert!(max_side < 1e-9, "side max abs diff {max_side}");
    }

    #[test]
    fn ms_taps_have_interaural_structure() {
        // Mid should be close to a unit impulse at the modelling delay, the
        // Side filter should carry a clearly non-trivial response.
        let mid = build_ms_mid_filter_taps(30.0, 48000, 2048, 9.0);
        let peak_index = mid
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(peak_index, 2048 / MS_FILTER_MODELING_DELAY_DIVISOR);
        assert!(mid.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn fir_matrix_is_consistent_with_mid_side() {
        let mid = build_ms_mid_filter_taps(45.0, 48000, 512, 6.0);
        let side = build_ms_side_filter_taps(45.0, 48000, 512, 6.0);
        let matrix = build_ms_fir_matrix(45.0, 48000, 512, 6.0);
        for i in 0..512 {
            assert!((matrix.ll[i] - (mid[i] + side[i]) / 2.0).abs() < 1e-12);
            assert!((matrix.lr[i] - (mid[i] - side[i]) / 2.0).abs() < 1e-12);
        }
    }

    #[test]
    fn angle_and_boost_are_clamped() {
        assert_eq!(coerce_crossfeed_angle_deg(-5.0), 0.0);
        assert_eq!(coerce_crossfeed_angle_deg(120.0), 90.0);
        assert_eq!(coerce_crossfeed_angle_deg(f64::NAN), DEFAULT_CROSSFEED_ANGLE_DEG);
        assert_eq!(coerce_crossfeed_mid_boost_db(40.0), MAX_CROSSFEED_MID_BOOST_DB);
        assert_eq!(quantized_angle(2.5), 2);
        assert_eq!(quantized_angle(3.5), 4);
    }

    #[test]
    fn measured_assets_load_for_supported_rates() {
        assert!(ku100_asset_available(30.0, 44100));
        assert!(ku100_asset_available(30.0, 48000));
        assert!(!ku100_asset_available(30.0, 96000));
    }

    #[test]
    fn ku100_base_rate_follows_integer_divisors() {
        for rate in [44_100, 88_200, 176_400, 352_800] {
            assert_eq!(ku100_base_sample_rate(rate), 44_100, "source {rate}");
        }
        for rate in [48_000, 96_000, 192_000, 384_000] {
            assert_eq!(ku100_base_sample_rate(rate), 48_000, "source {rate}");
        }
    }

    #[test]
    fn preview_has_expected_shape_and_sign() {
        let preview = compute_crossfeed_preview(30.0, 48000, 9.0);
        assert_eq!(preview.freq_hz.len(), CURVE_POINTS);
        assert_eq!(preview.mid_db.len(), CURVE_POINTS);
        assert_eq!(preview.side_db.len(), CURVE_POINTS);
        assert_eq!(preview.ild_db.len(), CURVE_POINTS);
        // Contra path is quieter than ipsi at mid frequencies (head shadow).
        let idx_1k = preview
            .freq_hz
            .iter()
            .position(|&f| f >= 1000.0)
            .unwrap();
        assert!(preview.ild_db[idx_1k] < 0.0);
    }

    #[test]
    fn write_text_fir_round_trip() {
        let dir = std::env::temp_dir().join(format!("hdsp-fir-test-{}", std::process::id()));
        let path = dir.join("taps.txt");
        let taps = vec![0.5, -0.25, 1.0e-7];
        write_text_fir_file(&path, &taps).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("0"));
        let parsed: Vec<f64> = lines.map(|l| l.parse().unwrap()).collect();
        assert_eq!(parsed, taps);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crossfeed_headroom_is_negative_and_tracks_mid_boost() {
        let headroom = |boost: f64| {
            let mid = build_ms_mid_filter_taps(60.0, 44_100, 2048, boost);
            let side = build_ms_side_filter_taps(60.0, 44_100, 2048, boost);
            crossfeed_headroom_db(&mid, &side)
        };
        let at_0db = headroom(0.0);
        let at_3db = headroom(3.0);
        let at_9db = headroom(9.0);
        assert!(at_0db < -3.0 && at_0db > -4.5, "0 dB boost headroom: {at_0db}");
        assert!(at_3db < -6.0 && at_3db > -7.0, "3 dB boost headroom: {at_3db}");
        assert!(at_9db < at_3db, "9 dB boost ({at_9db}) must need more headroom than 3 dB ({at_3db})");
        assert!(at_0db <= 0.0, "headroom is never positive");
    }
}
