# Fullscreen surface acquisition research

Continue the user's requested web-assisted investigation after the independently
traced D3D9 producer also freezes. Look for a fresh window-owned pixel source,
not more reader permutations on the stale allocation.

- [ ] Compare original Microsoft/AMD documentation for DirectComposition HWND
  surfaces, legacy DWM driver exports, presentation history, shared-resource
  opening and modern presentation APIs. Distinguish source access from timing.
- [ ] Check local signed DLL exports and installed SDK declarations read-only
  where online documentation leaves applicability uncertain. Do not invoke
  unknown ordinals, write shared surfaces, or intercept compositor queues.
- [ ] Evaluate independently sourced alternatives against native-exclusive,
  game-only, no-injection and no-WGC requirements. Treat presentation-changing
  workarounds as a different contract, not a passing exclusive capture result.
- [ ] Run a new bounded capture only if the evidence supplies a concrete new
  mechanism with appropriate ownership/protection semantics. Otherwise preserve
  the negative research result and exact missing prerequisite without claiming
  universal impossibility or manufacturing another variation of the same test.
- [ ] Review conclusions, update documentation/PR, verify CI and leave the app
  paused. Do not alter system settings, install drivers or elevate for this audit.
