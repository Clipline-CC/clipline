# Experimental window and fullscreen display capture

The user explicitly accepts PrintWindow for windowed/borderless capture and
whole-display capture for exclusive fullscreen. Keep Auto/WGC and explicit
Desktop Duplication behavior unchanged. Expose the combination as experimental;
do not describe display frames as isolated game frames.

- [ ] Validate read-only Windows exclusive-ownership signals against the existing
  mock's windowed, borderless and exclusive transitions. Geometry alone and
  unchanged pixel hashes must never classify a presentation mode. Record the
  limits of signals that do not identify the owner process.
- [ ] Add neutral failing tests for source decisions and before/after display
  guards (focus, identity, visibility, protection, monitor and ownership changes).
- [ ] Reuse the first-party PrintWindow worker protocol with bounded, supervised
  child execution; support resize and keep unsafe code in Windows modules.
- [ ] Add an explicitly selected experimental backend sharing the existing
  encoder device, clock, audio, replay and aspect-preserving conversion. Emit
  safe waiting frames on focus loss/transitions, with no WGC fallback.
- [ ] Wire settings, automatic game selection and truthful source diagnostics.
  Preserve default and persisted capture choices. Do not switch to primary when
  a game's monitor is unavailable.
- [ ] Exercise mock auto-start, window/borderless/fullscreen transitions, overlap,
  focus loss, minimize/restore, session/replay decode, audio and bounded resources.
  Treat unsupported presentation signals or stale window content as limitations,
  not successful capture. No claim of atomic desktop exclusion or tear freedom.
- [ ] Run workspace tests and fresh-cache Clippy, obtain independent review,
  update handoff/evidence, push the existing PR, verify Windows/Linux CI, and
  rebuild/open the app for testing.

No injection, elevation for recording, protection bypass, global display-policy
changes or borrowed shared-handle ownership changes. Any diagnostic compatibility
change is confined to a copied mock executable and restored after testing.
