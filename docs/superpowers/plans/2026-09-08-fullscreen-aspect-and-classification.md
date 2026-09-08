# Fullscreen presentation classification and aspect preservation

- [ ] Add failing neutral tests for fitting source/crop dimensions into a fixed
  even-sized NV12 output without stretching, including invalid/overflow inputs.
- [ ] Configure the D3D11 video processor destination and opaque black background;
  recompute after input resize and validate crops before conversion.
- [ ] Test GPU pixel output across wide/tall/source-resize cases, then a live
  fullscreen transition recording with audio/replay and matching-aspect controls.
- [ ] Download and verify official standalone PresentMon locally; attempt a bounded
  PID-scoped non-elevated trace first. No service/driver/global runtime installation.
- [ ] If telemetry is available, compare default vs disabled Fullscreen Optimizations
  on a copied mock only, with fresh window-only capture probes. Restore compatibility
  settings. Distinguish actual display ownership from GetFullscreenState reporting.
- [ ] Preserve classification negatives/permission prerequisites accurately. Do not
  classify monitor capture or FSO/borderless conversion as strict exclusive isolation.
- [ ] Review, run CI=1 workspace tests and warning-denied Clippy, rebuild/open the app,
  update docs/handoff and PR, push and verify Windows/Linux CI.

No WGC, injection, protected-window bypass, production experimental-backend integration,
borrowed shared-handle closes, or copied third-party capture implementation.