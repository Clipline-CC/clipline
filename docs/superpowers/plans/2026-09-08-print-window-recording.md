# Native PrintWindow recording experiment

The screenshot diagnostic found fresh flip-model windowed/borderless content.
Determine whether it survives Clipline's actual encoder, audio and replay pipeline.
Keep this example separate from the app and every production backend. No WGC,
injection, display fallback, protection bypass or presentation-mode changes.

- [ ] Add failing neutral tests for a bounded frame protocol and mock motion
  acceptance; reject malformed geometry, timestamps and blank/stalled frames.
- [ ] Implement a native child process that alone calls synchronous PrintWindow
  with SDK flag 2. Check held process identity, affinity, visibility and geometry
  before and after reads; use a top-down DIB, GdiFlush and client-area cropping.
  Stop on resize/minimize in this first fixed-resolution prototype.
- [ ] Supervise one requested frame at a time with a bounded channel, a two-second
  read deadline and owned-child cleanup. Upload BGRA on the encoder's D3D device.
  Use one QPC RelativeClock with WASAPI, existing AMF encoding and Recorder APIs;
  create a session and a bounded trailing replay in a new evidence directory.
  Record observed capture times, counters and throughput without claiming present
  synchronization or frame-perfect A/V sync.
- [ ] Exercise windowed/borderless mocks, overlap, negative exclusive/minimize/
  resize cases and sustained audio/video decoding. Measure process memory and
  capture latency, inspect colors and actual visible borders, preserve failures.
- [ ] Review, document measured limits, run workspace tests with CI=1 to respect
  the no-WGC constraint, run warning-denied Clippy, rebuild/open Clipline and push
  the existing PR with both CI platforms green. App automatic game capture remains
  blocked until a viable backend is established; this does not integrate it.
