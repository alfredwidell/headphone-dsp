//! Parametric EQ model: band data, RBJ biquad responses matching CamillaDSP's
//! `Biquad` filter types, response curves for the UI, and EqualizerAPO/AutoEQ
//! text import/export.

use num_complex::Complex64;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

pub const FREQ_MIN_HZ: f64 = 20.0;
pub const FREQ_MAX_HZ: f64 = 20_000.0;
pub const GAIN_MIN_DB: f64 = -24.0;
pub const GAIN_MAX_DB: f64 = 24.0;
pub const Q_MIN: f64 = 0.1;
pub const Q_MAX: f64 = 20.0;
pub const CURVE_POINTS: usize = 256;
pub const MAX_BANDS: usize = 12;
const IMPORT_PREAMP_MIN_DB: f64 = -18.0;
const IMPORT_PREAMP_MAX_DB: f64 = 12.0;
const IMPORT_BAND_GAIN_MAX_DB: f64 = 18.0;

/// Canonical band type names, as used by the UI's `<select>`.
pub const BAND_TYPES: [&str; 6] = ["bell", "low_shelf", "high_shelf", "high_pass", "low_pass", "notch"];

/// Map any accepted spelling of a band type to its canonical name.
/// `highpass`/`high_pass` and `lowpass`/`low_pass` are both accepted.
pub fn canonical_band_type(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "bell" => Some("bell"),
        "low_shelf" => Some("low_shelf"),
        "high_shelf" => Some("high_shelf"),
        "high_pass" | "highpass" => Some("high_pass"),
        "low_pass" | "lowpass" => Some("low_pass"),
        "notch" => Some("notch"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EqBand {
    pub enabled: bool,
    pub band_type: String,
    pub freq_hz: f64,
    pub gain_db: f64,
    pub q: f64,
}

impl Default for EqBand {
    fn default() -> Self {
        Self {
            enabled: true,
            band_type: "bell".to_string(),
            freq_hz: 1000.0,
            gain_db: 0.0,
            q: 1.0,
        }
    }
}

impl EqBand {
    pub fn new(band_type: &str, freq_hz: f64, gain_db: f64, q: f64) -> Self {
        Self {
            enabled: true,
            band_type: band_type.to_string(),
            freq_hz,
            gain_db,
            q,
        }
    }

    /// Clamp numeric fields to their legal ranges and canonicalize the band
    /// type (unknown types become `bell`). Used everywhere a band is consumed.
    pub fn sanitized(&self) -> EqBand {
        let band_type = canonical_band_type(&self.band_type).unwrap_or("bell").to_string();
        let finite = |v: f64, fallback: f64| if v.is_finite() { v } else { fallback };
        EqBand {
            enabled: self.enabled,
            band_type,
            freq_hz: finite(self.freq_hz, 1000.0).clamp(FREQ_MIN_HZ, FREQ_MAX_HZ),
            gain_db: finite(self.gain_db, 0.0).clamp(GAIN_MIN_DB, GAIN_MAX_DB),
            q: finite(self.q, 1.0).clamp(Q_MIN, Q_MAX),
        }
    }

    /// CamillaDSP `Biquad` `type` parameter for this band.
    pub fn camilla_type(&self) -> &'static str {
        match canonical_band_type(&self.band_type).unwrap_or("bell") {
            "low_shelf" => "Lowshelf",
            "high_shelf" => "Highshelf",
            "high_pass" => "Highpass",
            "low_pass" => "Lowpass",
            "notch" => "Notch",
            _ => "Peaking",
        }
    }

    /// Whether the filter type takes a gain parameter.
    pub fn uses_gain(&self) -> bool {
        matches!(
            canonical_band_type(&self.band_type),
            Some("bell") | Some("low_shelf") | Some("high_shelf")
        )
    }
}

/// Normalised biquad (a0 == 1): H(z) = (b0 + b1 z^-1 + b2 z^-2) / (1 + a1 z^-1 + a2 z^-2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Biquad {
    /// Complex response at `freq_hz` for a stream at `sample_rate`.
    pub fn response(&self, freq_hz: f64, sample_rate: f64) -> Complex64 {
        let w = 2.0 * PI * freq_hz / sample_rate;
        let z1 = Complex64::from_polar(1.0, -w);
        let z2 = z1 * z1;
        let num = Complex64::new(self.b0, 0.0) + z1 * self.b1 + z2 * self.b2;
        let den = Complex64::new(1.0, 0.0) + z1 * self.a1 + z2 * self.a2;
        num / den
    }
}

/// RBJ Audio EQ Cookbook coefficients, using the Q (not slope) convention
/// for shelves, i.e. `alpha = sin(w0) / (2Q)`. Frequencies are capped below
/// 0.49 * fs so the design stays stable at low sample rates.
pub fn biquad_coefficients(band: &EqBand, sample_rate: f64) -> Biquad {
    let band = band.sanitized();
    let fs = sample_rate.max(1.0);
    let f0 = band.freq_hz.min(0.49 * fs).max(1.0);
    let w0 = 2.0 * PI * f0 / fs;
    let (sin_w, cos_w) = w0.sin_cos();
    let alpha = sin_w / (2.0 * band.q);
    let a_lin = 10f64.powf(band.gain_db / 40.0);
    let sqrt_a = a_lin.sqrt();

    let (b0, b1, b2, a0, a1, a2) = match band.camilla_type() {
        "Peaking" => (
            1.0 + alpha * a_lin,
            -2.0 * cos_w,
            1.0 - alpha * a_lin,
            1.0 + alpha / a_lin,
            -2.0 * cos_w,
            1.0 - alpha / a_lin,
        ),
        "Lowshelf" => (
            a_lin * ((a_lin + 1.0) - (a_lin - 1.0) * cos_w + 2.0 * sqrt_a * alpha),
            2.0 * a_lin * ((a_lin - 1.0) - (a_lin + 1.0) * cos_w),
            a_lin * ((a_lin + 1.0) - (a_lin - 1.0) * cos_w - 2.0 * sqrt_a * alpha),
            (a_lin + 1.0) + (a_lin - 1.0) * cos_w + 2.0 * sqrt_a * alpha,
            -2.0 * ((a_lin - 1.0) + (a_lin + 1.0) * cos_w),
            (a_lin + 1.0) + (a_lin - 1.0) * cos_w - 2.0 * sqrt_a * alpha,
        ),
        "Highshelf" => (
            a_lin * ((a_lin + 1.0) + (a_lin - 1.0) * cos_w + 2.0 * sqrt_a * alpha),
            -2.0 * a_lin * ((a_lin - 1.0) + (a_lin + 1.0) * cos_w),
            a_lin * ((a_lin + 1.0) + (a_lin - 1.0) * cos_w - 2.0 * sqrt_a * alpha),
            (a_lin + 1.0) - (a_lin - 1.0) * cos_w + 2.0 * sqrt_a * alpha,
            2.0 * ((a_lin - 1.0) - (a_lin + 1.0) * cos_w),
            (a_lin + 1.0) - (a_lin - 1.0) * cos_w - 2.0 * sqrt_a * alpha,
        ),
        "Highpass" => (
            (1.0 + cos_w) / 2.0,
            -(1.0 + cos_w),
            (1.0 + cos_w) / 2.0,
            1.0 + alpha,
            -2.0 * cos_w,
            1.0 - alpha,
        ),
        "Lowpass" => (
            (1.0 - cos_w) / 2.0,
            1.0 - cos_w,
            (1.0 - cos_w) / 2.0,
            1.0 + alpha,
            -2.0 * cos_w,
            1.0 - alpha,
        ),
        // Notch
        _ => (1.0, -2.0 * cos_w, 1.0, 1.0 + alpha, -2.0 * cos_w, 1.0 - alpha),
    };

    Biquad {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

/// `points` log-spaced frequencies from `min` to `max` inclusive.
pub fn log_frequency_grid(points: usize, min: f64, max: f64) -> Vec<f64> {
    if points == 1 {
        return vec![min];
    }
    let ratio = (max / min).ln();
    (0..points)
        .map(|i| min * (ratio * i as f64 / (points - 1) as f64).exp())
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct PeqResponse {
    pub freq_hz: Vec<f64>,
    /// Sum of enabled bands plus the effective preamp, in dB. All zeros when
    /// EQ is disabled.
    pub composite_db: Vec<f64>,
    /// Maximum positive gain of the enabled bands (dB, floored at 0). Zero
    /// when EQ is disabled.
    pub raw_peak_db: f64,
    pub effective_preamp_db: f64,
    /// One curve (dB) per entry of `bands`, in the same order. Disabled bands
    /// (and every band when EQ is disabled) are all zeros.
    pub per_band_db: Vec<Vec<f64>>,
}

/// Compute the EQ curve. Bands are evaluated at `sample_rate` so the curve
/// matches what CamillaDSP will actually produce.
///
/// Effective preamp: when `auto_preamp` is set it is
/// `min(-raw_peak_db - 0.5, 0) + preamp_db`, i.e. attenuate by the largest
/// boost plus 0.5 dB of headroom, never amplify; otherwise `preamp_db`.
pub fn compute_peq_response(
    bands: &[EqBand],
    preamp_db: f64,
    auto_preamp: bool,
    sample_rate: u32,
    eq_enabled: bool,
) -> PeqResponse {
    let freq_hz = log_frequency_grid(CURVE_POINTS, FREQ_MIN_HZ, FREQ_MAX_HZ);
    let fs = sample_rate.max(1) as f64;

    let mut per_band_db = Vec::with_capacity(bands.len());
    let mut sum_db = vec![0.0f64; freq_hz.len()];
    for band in bands {
        if !(eq_enabled && band.enabled) {
            per_band_db.push(vec![0.0; freq_hz.len()]);
            continue;
        }
        let bq = biquad_coefficients(band, fs);
        let curve: Vec<f64> = freq_hz
            .iter()
            .map(|&f| 20.0 * bq.response(f, fs).norm().max(1e-12).log10())
            .collect();
        for (acc, v) in sum_db.iter_mut().zip(curve.iter()) {
            *acc += v;
        }
        per_band_db.push(curve);
    }

    let raw_peak_db = sum_db.iter().copied().fold(0.0f64, f64::max);
    let effective_preamp_db = if auto_preamp {
        (-raw_peak_db).min(0.0) + preamp_db
    } else {
        preamp_db
    };
    let composite_db = sum_db.iter().map(|v| v + effective_preamp_db).collect();

    PeqResponse {
        freq_hz,
        composite_db,
        raw_peak_db,
        effective_preamp_db,
        per_band_db,
    }
}

/// Filter codes accepted on import (EqualizerAPO and the UI's AutoEQ parser).
fn type_from_code(code: &str) -> Option<&'static str> {
    match code.to_ascii_uppercase().as_str() {
        "PK" | "PEQ" => Some("bell"),
        "LS" | "LSC" => Some("low_shelf"),
        "HS" | "HSC" => Some("high_shelf"),
        "HP" | "HPQ" => Some("high_pass"),
        "LP" | "LPQ" => Some("low_pass"),
        "NO" | "NOTCH" => Some("notch"),
        _ => None,
    }
}

/// Export codes, matching the UI's exporter.
fn code_from_type(band_type: &str) -> &'static str {
    match canonical_band_type(band_type).unwrap_or("bell") {
        "low_shelf" => "LS",
        "high_shelf" => "HS",
        "high_pass" => "HPQ",
        "low_pass" => "LPQ",
        "notch" => "NO",
        _ => "PK",
    }
}

/// Parse an EqualizerAPO / AutoEQ / Oratory1990 style text block.
///
/// Recognised lines:
/// - `Preamp: -5.4 dB` (must be within -18..+12 dB)
/// - `Filter N: ON|OFF <TYPE> Fc <hz> Hz [Gain <db> dB] [Q <q>]`
///   where TYPE is PK/PEQ, LS/LSC, HS/HSC, LP/LPQ, HP/HPQ or NO/NOTCH.
///   Shelves and bells need a gain; a missing Q defaults to 0.7071.
///
/// Blank lines, comments (`#`) and unrelated lines are ignored. A malformed
/// or out-of-range line is an error that names its line number. At most
/// [`MAX_BANDS`] filters are accepted.
pub fn parse_autoeq_text(text: &str) -> Result<(Option<f64>, Vec<EqBand>), String> {
    let mut preamp: Option<f64> = None;
    let mut bands = Vec::new();

    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();

        if lower.starts_with("preamp") {
            let value = line
                .split_once(':')
                .map(|(_, rest)| rest)
                .ok_or_else(|| format!("line {line_no}: malformed Preamp line"))?;
            let number = value
                .split_whitespace()
                .next()
                .ok_or_else(|| format!("line {line_no}: missing Preamp value"))?
                .trim_end_matches("dB")
                .trim_end_matches("db")
                .parse::<f64>()
                .map_err(|_| format!("line {line_no}: invalid Preamp value"))?;
            if !(IMPORT_PREAMP_MIN_DB..=IMPORT_PREAMP_MAX_DB).contains(&number) {
                return Err(format!(
                    "line {line_no}: Preamp must be between {IMPORT_PREAMP_MIN_DB} and +{IMPORT_PREAMP_MAX_DB} dB"
                ));
            }
            preamp = Some(number);
            continue;
        }

        if lower.starts_with("filter") {
            let band = parse_filter_line(line).map_err(|e| format!("line {line_no}: {e}"))?;
            bands.push(band);
            if bands.len() > MAX_BANDS {
                return Err(format!("a preset can contain at most {MAX_BANDS} filters"));
            }
        }
    }

    if bands.is_empty() && preamp.is_none() {
        return Err("no supported filters or preamp found; paste EqualizerAPO / AutoEQ text".to_string());
    }
    Ok((preamp, bands))
}

fn parse_filter_line(line: &str) -> Result<EqBand, String> {
    let (_, rest) = line
        .split_once(':')
        .ok_or_else(|| "missing ':' after Filter".to_string())?;
    let tokens: Vec<&str> = rest.split_whitespace().collect();

    let mut enabled = true;
    let mut band_type: Option<&'static str> = None;
    let mut freq: Option<f64> = None;
    let mut gain: Option<f64> = None;
    let mut q = 0.7071;

    let number_after = |i: usize, name: &str| -> Result<f64, String> {
        tokens
            .get(i + 1)
            .ok_or_else(|| format!("'{name}' without value"))?
            .parse::<f64>()
            .map_err(|_| format!("invalid number after '{name}'"))
    };
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        match token.to_ascii_uppercase().as_str() {
            "ON" => enabled = true,
            "OFF" => enabled = false,
            "FC" => {
                freq = Some(number_after(i, "Fc")?);
                i += 1;
            }
            "GAIN" => {
                gain = Some(number_after(i, "Gain")?);
                i += 1;
            }
            "Q" => {
                q = number_after(i, "Q")?;
                i += 1;
            }
            "HZ" | "DB" => {}
            _ => {
                let t = type_from_code(token)
                    .ok_or_else(|| format!("unsupported filter token '{token}'"))?;
                band_type = Some(t);
            }
        }
        i += 1;
    }

    let band_type = band_type.ok_or_else(|| "missing filter type".to_string())?;
    let freq = freq.ok_or_else(|| "missing 'Fc' frequency".to_string())?;
    let uses_gain = matches!(band_type, "bell" | "low_shelf" | "high_shelf");
    let gain = match (uses_gain, gain) {
        (true, Some(g)) => g,
        (true, None) => return Err("missing 'Gain' for this filter type".to_string()),
        (false, _) => 0.0,
    };
    if !(FREQ_MIN_HZ..=FREQ_MAX_HZ).contains(&freq)
        || gain.abs() > IMPORT_BAND_GAIN_MAX_DB
        || !(Q_MIN..=Q_MAX).contains(&q)
    {
        return Err(format!(
            "filter values out of range (20-20000 Hz, ±{IMPORT_BAND_GAIN_MAX_DB} dB, Q {Q_MIN}-{Q_MAX})"
        ));
    }
    Ok(EqBand {
        enabled,
        band_type: band_type.to_string(),
        freq_hz: freq,
        gain_db: gain,
        q,
    })
}

/// Export as EqualizerAPO text. `preamp_db` is written as given (the caller
/// passes the effective preamp). Disabled bands are written as `OFF` so a
/// round trip through [`parse_autoeq_text`] preserves them.
pub fn export_autoeq(preamp_db: f64, bands: &[EqBand]) -> String {
    let mut out = format!("Preamp: {preamp_db:.2} dB\n");
    for (index, band) in bands.iter().enumerate() {
        let band = band.sanitized();
        let state = if band.enabled { "ON" } else { "OFF" };
        let mut line = format!(
            "Filter {}: {} {} Fc {:.1} Hz",
            index + 1,
            state,
            code_from_type(&band.band_type),
            band.freq_hz
        );
        if band.uses_gain() {
            line.push_str(&format!(" Gain {:.2} dB", band.gain_db));
        }
        line.push_str(&format!(" Q {:.3}", band.q));
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 48_000;

    fn db_at(band: &EqBand, freq: f64) -> f64 {
        let bq = biquad_coefficients(band, FS as f64);
        20.0 * bq.response(freq, FS as f64).norm().log10()
    }

    #[test]
    fn bell_plus_six_db_at_1khz() {
        let band = EqBand::new("bell", 1000.0, 6.0, 1.0);
        assert!((db_at(&band, 1000.0) - 6.0).abs() < 1e-6, "got {}", db_at(&band, 1000.0));
        assert!(db_at(&band, 0.0001).abs() < 1e-6, "DC should be 0 dB");
    }

    #[test]
    fn bell_cut_is_symmetric_in_gain() {
        let band = EqBand::new("bell", 2000.0, -4.0, 2.0);
        assert!((db_at(&band, 2000.0) + 4.0).abs() < 1e-6);
    }

    #[test]
    fn low_shelf_reaches_gain_at_dc_and_zero_at_nyquist() {
        let band = EqBand::new("low_shelf", 200.0, 5.0, 0.707);
        assert!((db_at(&band, 1.0) - 5.0).abs() < 1e-3);
        assert!(db_at(&band, 23_900.0).abs() < 1e-2);
    }

    #[test]
    fn high_shelf_reaches_gain_at_nyquist_and_zero_at_dc() {
        let band = EqBand::new("high_shelf", 8000.0, -3.0, 0.707);
        assert!((db_at(&band, 23_990.0) + 3.0).abs() < 1e-2);
        assert!(db_at(&band, 1.0).abs() < 1e-3);
    }

    #[test]
    fn pass_filters_accept_both_spellings_and_have_expected_corners() {
        let lp = EqBand::new("low_pass", 1000.0, 0.0, 0.707);
        let hp = EqBand::new("highpass", 1000.0, 0.0, 0.707);
        assert_eq!(hp.sanitized().band_type, "high_pass");
        assert_eq!(lp.sanitized().band_type, "low_pass");
        assert_eq!(hp.camilla_type(), "Highpass");
        assert_eq!(lp.camilla_type(), "Lowpass");
        assert!(db_at(&lp, 1.0).abs() < 1e-6, "lowpass passes DC");
        assert!(db_at(&hp, 23_990.0).abs() < 1e-2, "highpass passes Nyquist");
        assert!((db_at(&lp, 1000.0) + 3.01).abs() < 0.05, "Butterworth -3 dB corner");
    }

    #[test]
    fn notch_is_a_null_at_centre() {
        let band = EqBand::new("notch", 1000.0, 0.0, 2.0);
        assert!(db_at(&band, 1000.0) < -80.0);
        assert!(db_at(&band, 1.0).abs() < 1e-3);
    }

    #[test]
    fn composite_response_applies_auto_preamp() {
        let bands = vec![EqBand::new("bell", 1000.0, 6.0, 1.0)];
        let resp = compute_peq_response(&bands, 0.0, true, FS, true);
        assert_eq!(resp.freq_hz.len(), CURVE_POINTS);
        assert!((resp.raw_peak_db - 6.0).abs() < 0.05);
        assert!((resp.effective_preamp_db + 6.0).abs() < 0.05);
        assert_eq!(resp.per_band_db.len(), 1);

        let manual = compute_peq_response(&bands, -2.0, false, FS, true);
        assert!((manual.effective_preamp_db + 2.0).abs() < 1e-12);
    }

    #[test]
    fn per_band_curves_follow_input_indices() {
        let mut off = EqBand::new("bell", 500.0, 6.0, 1.0);
        off.enabled = false;
        let on = EqBand::new("bell", 2000.0, 3.0, 1.0);
        let resp = compute_peq_response(&[off, on], 0.0, false, FS, true);
        assert_eq!(resp.per_band_db.len(), 2, "one curve per input band");
        assert!(resp.per_band_db[0].iter().all(|v| *v == 0.0), "disabled band is zeros");
        let idx_2k = resp.freq_hz.iter().position(|&f| f >= 2000.0).unwrap();
        assert!((resp.per_band_db[1][idx_2k] - 3.0).abs() < 0.1);
    }

    #[test]
    fn eq_disabled_zeroes_composite_and_peak() {
        let bands = vec![EqBand::new("bell", 1000.0, 6.0, 1.0)];
        let resp = compute_peq_response(&bands, 0.0, false, FS, false);
        assert!(resp.composite_db.iter().all(|v| *v == 0.0));
        assert_eq!(resp.raw_peak_db, 0.0);
        assert!(resp.per_band_db[0].iter().all(|v| *v == 0.0));
    }

    #[test]
    fn autoeq_parse_handles_all_types_and_round_trips() {
        let text = "\
# AutoEQ sample
Preamp: -5.4 dB
Filter 1: ON PK Fc 105 Hz Gain 5.5 dB Q 0.71
Filter 2: ON LSC Fc 80 Hz Gain 3.0 dB Q 0.70
Filter 3: ON HSC Fc 10000 Hz Gain -2.0 dB Q 0.70
Filter 4: OFF PK Fc 3000 Hz Gain 1.0 dB Q 2.0
Filter 5: ON LP Fc 18000 Hz Q 0.71
Filter 6: ON HPQ Fc 30 Hz Q 0.50
Filter 7: ON NO Fc 500 Hz Q 1.0
";
        let (preamp, bands) = parse_autoeq_text(text).expect("parse");
        assert_eq!(preamp, Some(-5.4));
        assert_eq!(bands.len(), 7);
        assert_eq!(bands[0].band_type, "bell");
        assert_eq!(bands[0].freq_hz, 105.0);
        assert_eq!(bands[0].gain_db, 5.5);
        assert_eq!(bands[1].band_type, "low_shelf");
        assert_eq!(bands[2].band_type, "high_shelf");
        assert_eq!(bands[2].gain_db, -2.0);
        assert!(!bands[3].enabled);
        assert_eq!(bands[4].band_type, "low_pass");
        assert_eq!(bands[4].gain_db, 0.0);
        assert_eq!(bands[5].band_type, "high_pass");
        assert_eq!(bands[5].q, 0.5);
        assert_eq!(bands[6].band_type, "notch");

        let exported = export_autoeq(preamp.unwrap(), &bands);
        let (preamp2, bands2) = parse_autoeq_text(&exported).expect("re-parse");
        assert_eq!(preamp2, Some(-5.4));
        assert_eq!(bands2.len(), bands.len());
        for (a, b) in bands.iter().zip(bands2.iter()) {
            assert_eq!(a.enabled, b.enabled);
            assert_eq!(a.band_type, b.band_type);
            assert!((a.freq_hz - b.freq_hz).abs() < 0.06);
            assert!((a.gain_db - b.gain_db).abs() < 0.006);
            assert!((a.q - b.q).abs() < 0.0006);
        }
    }

    #[test]
    fn autoeq_parse_rejects_bad_input() {
        assert!(parse_autoeq_text("Filter 1: ON XX Fc 100 Hz Gain 1 dB Q 1").is_err());
        assert!(parse_autoeq_text("Filter 1: ON PK Gain 1 dB Q 1").is_err());
        assert!(parse_autoeq_text("Filter 1: ON PK Fc 100 Hz Q 1").is_err(), "bell needs gain");
        assert!(parse_autoeq_text("Filter 1: ON PK Fc 5 Hz Gain 1 dB Q 1").is_err(), "freq range");
        assert!(parse_autoeq_text("Filter 1: ON PK Fc 100 Hz Gain 30 dB Q 1").is_err(), "gain range");
        assert!(parse_autoeq_text("Preamp: 20 dB").is_err(), "preamp range");
        assert!(parse_autoeq_text("Preamp: loud dB").is_err());
        assert!(parse_autoeq_text("Channel: L").is_err(), "nothing supported");
        let many: String = (0..13)
            .map(|i| format!("Filter {}: ON HP Fc 80 Hz Q 0.7\n", i + 1))
            .collect();
        assert!(parse_autoeq_text(&many).is_err(), "at most 12 filters");
    }

    #[test]
    fn log_grid_spans_range() {
        let grid = log_frequency_grid(CURVE_POINTS, FREQ_MIN_HZ, FREQ_MAX_HZ);
        assert!((grid[0] - 20.0).abs() < 1e-9);
        assert!((grid[CURVE_POINTS - 1] - 20_000.0).abs() < 1e-6);
        assert!(grid.windows(2).all(|w| w[1] > w[0]));
    }
}
