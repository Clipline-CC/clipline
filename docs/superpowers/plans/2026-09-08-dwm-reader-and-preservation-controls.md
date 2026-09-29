# DWM reader interoperability and buffer preservation controls

Continue the experimental window-only fullscreen investigation. Keep production
recording unchanged and do not use WGC, monitor capture, injection, protection
bypass, or a cooperating game frame-export implementation.

- [ ] Inspect the local signed user32 export's calling sequence read-only to
  check the existing ABI assumption. Do not alter or redistribute system DLLs.
- [ ] Build an original bounded local D3D9Ex readback diagnostic using the DWM
  shared handle, with same-adapter selection, window identity/affinity checks,
  borrowed-handle ownership, independent readback and saved pixel evidence. Use
  D3D11 only to obtain validated dimensions of that same shared allocation if
  required to open it through D3D9Ex; never substitute another capture source.
- [ ] Establish a moving blt positive control before interpreting flip/fullscreen
  results. Compare the alternate reader with the existing D3D11 DWM probe and
  PrintWindow, with external deadlines around synchronous calls.
- [ ] Build a separate mock variant that changes FLIP_DISCARD to FLIP_SEQUENTIAL
  only. Preserve animation/audio and omit any cooperating capture behavior.
  Compare the same geometry through borderless/exclusive/borderless, recording
  actual presentation with the existing authorized bounded PresentMon workflow.
- [ ] Treat fixture-specific success as a lead requiring further validation, not
  a fix for unmodified games. If the controls fail, preserve that evidence and
  its limits. Restore copied-fixture settings, stop helpers, update documentation
  and PR, and verify CI. Keep the app open and paused.
