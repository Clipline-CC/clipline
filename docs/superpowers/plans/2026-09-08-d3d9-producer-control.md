# Independent D3D9 producer fullscreen control

Test the user's approved next hypothesis: a different rendering API in the mock,
not merely a different reader of the same D3D11-produced DWM allocation. Keep all
capture mechanisms outside production and retain the no-WGC/injection/display
fallback constraints.

- [ ] Build an original bounded local D3D9Ex rendering fixture using the existing
  first-party mock's colors, visible frame counter and stereo audio. Use hardware
  rendering, log adapter and actual presentation parameters, and provide no frame
  export or capture-specific cooperation. Fail visibly on setup/reset errors.
- [ ] Establish windowed moving-pixel positive controls with the existing DWM,
  D3D9 readback and PrintWindow diagnostics before interpreting fullscreen data.
- [ ] Compare borderless, exclusive and restored borderless at constant geometry,
  with source identity/foreground checks, saved pixels and bounded subprocesses.
  Run the already authorized signed PresentMon trace with explicit UAC notice;
  require concurrent presentation evidence for native-exclusive classification.
- [ ] If a source passes freshness, investigate that specific success before
  claiming compatibility or recording support. Otherwise report the negative
  result without universal impossibility claims or a production source change.
- [ ] Restore copied-fixture settings, stop helpers, preserve local evidence,
  obtain independent review, update research/handoff/PR, and check CI. Leave
  Clipline open and capture paused.
