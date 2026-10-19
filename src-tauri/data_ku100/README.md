This directory stores repo-local KU100-mode crossfeed FIR assets.

Expected runtime file layout:
- 44100/angle_000.json
- 44100/angle_001.json
- ...
- 48000/angle_090.json

Each JSON file should contain keys: angle_deg, sample_rate, ll, rr, lr, rl.

Current status:

- The checked-in angle_*.json files are real KU100-derived horizontal-plane FIR
  assets baked offline from KU100 SOFA HRIR data for integer angles 0-90°.
- Runtime supports baked assets for both 44100 Hz and 48000 Hz.
- Missing or incompatible angle files still fall back to the analytic model,
  with a runtime warning.

The bake pipeline lives in `tools/headphone_tuning/crossfeed_ku100/bake_hrir.py`.
