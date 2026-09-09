# Hybrid window recording canvas

The supplied 1920x540 replay contains a centered 960x540 game and black sides;
the user confirms the game window itself contains only gameplay. Hybrid startup
currently seeds the encoder with monitor dimensions, imposing the monitor aspect
ratio on isolated window capture. Use the initial valid client dimensions instead.

- [ ] Add a failing neutral regression for a 16:9 client on a 32:9 display,
  plus same-aspect/full-display and unavailable-client cases.
- [ ] Seed the hybrid canvas from validated client geometry. Preserve the existing
  monitor fallback when initial client geometry is unavailable, black waiting,
  fullscreen guards, shared aspect-preserving conversion and fixed session size.
- [ ] Verify the corrected client aspect fills its recording canvas; retain
  letterboxing for actual aspect changes later in a session. Do not crop pixels,
  stretch the game or infer content bounds from black pixel values.
- [ ] Run workspace tests and fresh capture-cache warning-denied Clippy, document
  evidence and startup/resize limitations, push a fix PR with Windows/Ubuntu CI,
  and leave the normal app open for testing. No release is requested by this fix.
