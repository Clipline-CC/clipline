# Hybrid canvas headroom review revision

PR #201's direct client-sized seed accidentally caps all later frames at the
initial window resolution. Preserve a monitor-sized budget with the game aspect
instead. This intentionally permits upscaling small windows; a fixed aspect canvas
still letterboxes a later different-aspect display source. It does not create
detail, dynamically resize an encoder, or guarantee a settled startup aspect.

- [ ] Replace the trivial selector with bounded client-aspect fitting inside the
  monitor, a 64-pixel minimum on both client and fitted dimensions, and explicit
  fallback reasons. Reproduce the 1280x720-to-5120x1440 ceiling defect in tests.
- [ ] Test canvas decisions/aspect/headroom, odd monitor bounds and tiny/extreme
  clients without redundant pixel-converter assertions. Keep fallback semantics.
- [ ] Emit diagnostic client/display/canvas dimensions and the decision reason;
  log actual encoder input/output dimensions in the app.
- [ ] Use unambiguous size names; make desktop fixture nonactivating and explicitly
  ignored on CI, run it locally, and format touched Rust files with 2024 style.
- [ ] Validate actual window-to-fullscreen switching on the physical fixture,
  document exact machine/output dimensions and distinguish it from ultrawide
  policy coverage. Remove manual startup advice and state remaining limits.
- [ ] Run workspace tests, fresh-cache Clippy and isolated formatting checks;
  obtain review, update handoff/report/PR, push and verify Windows/Ubuntu CI.
  Leave the app open. Do not merge or publish a new release in this revision.
