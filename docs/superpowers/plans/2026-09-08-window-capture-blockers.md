# Diagnose Windows 10 window-capture blockers

The native mock matrix isolates stale DWM surfaces for flip presentation and DXGI
exclusive fullscreen. Investigate practical window-only alternatives while keeping
production recording unchanged. No WGC, injection, capture-protection bypass,
borrowed-handle closes, forced driver changes or implicit desktop capture.

- [ ] Check primary Microsoft presentation/API documentation and independently
  review the probe's handle/device/copy path. Separate inferred surface limitations
  from demonstrated implementation defects.
- [ ] Use isolated helpers with external timeouts to compare PrintWindow default
  and SDK PW_RENDERFULLCONTENT against the unmodified GPU mocks. Validate window
  identity and capture affinity before every sample. Inspect motion/color/overlap,
  measure blocking latency and test exclusive state separately.
- [ ] Test a documented DWM thumbnail into a probe-owned window, then read that
  window's shared surface separately. A visible live thumbnail is not a capturable
  texture; never substitute desktop readback of the helper without calling it a
  separate display test.
- [ ] Fix the reproduced acceptance defect: a motion-required probe run must fail
  when the animated target yields only identical pixels, while preserving BMP/CSV
  evidence. Add neutral failing tests first; keep static-image sampling available.
- [ ] Record actionable results and remaining limits, run workspace tests with
  device tests skipped to preserve no-WGC scope, warning-denied Clippy, build/open
  Clipline, obtain review, and update PR #200 with CI green.

Only extend an experimental path if it produces fresh, isolated content under the
mock acceptance matrix. No integration decision follows merely from an API success
return, a changing update ID or a live on-screen thumbnail.
