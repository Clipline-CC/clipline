# Standalone mock games for Windows capture acceptance

Build first-party test executables with D3D11 visuals and generated stereo audio.
Register their executable paths through Clipline's custom-game UI and inspect real
automatic detection, capture, replay and stop behavior. No injection, protected
window bypass, GPL source, or production DWM integration.

- [ ] Add a Windows-only fixture and build script producing distinct blt/flip
  executable identities. Use a hardware D3D11 device, moving SDR color blocks,
  frame/audio counters, bounded lifetime and stereo tones. Surface failures clearly.
- [ ] Exercise windowed and borderless modes; offer an explicit exclusive mode
  for separate tests. Label the renderer and actual mode; do not equate fixture
  compatibility with a real game/engine/anti-cheat result.
- [ ] Register each executable in Clipline, enable game detection and games-only
  waiting, then launch/close/relaunch the fixture and try F6 replay saving.
  Preserve the no-WGC requirement unless the user explicitly authorizes a separate
  WGC diagnostic. Expected DXGI window-source rejection is a failed end-to-end
  acceptance result, not successful automatic game capture.
- [ ] Probe DWM against both presentation modes independently; inspect colors,
  motion and window overlap. Keep raw desktop/window-title evidence local.
- [ ] Record results and limitations, restore a reviewable app configuration, run
  appropriate build/quality checks, obtain code review and update the existing PR.

The fixture is a reversible manual hardware test tool. Validate it by building and
running its real render/audio paths rather than adding tests that restate drawing
implementation. Existing neutral capture-policy tests guard WGC prohibition.
