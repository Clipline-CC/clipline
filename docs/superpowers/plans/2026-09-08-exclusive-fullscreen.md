# Exclusive fullscreen capture investigation

Prioritize the exclusive failure without changing the production recorder, using
the existing Windows 10 AMD machine, registered mocks and audio endpoint.

- [ ] Review mock fullscreen transitions against Microsoft DXGI requirements.
  Distinguish reported fullscreen state from physical exclusive scanout/FSO.
- [ ] Compare PrintWindow 0/1/2/3 with fresh helpers after fullscreen settles;
  client-only calls must use client-sized DIBs and zero crop offset. Add neutral
  layout tests before implementation. Preserve negative and return-to-windowed
  controls; do not reinterpret frozen or invalid pixels as success.
- [ ] Run an explicitly labeled Desktop Duplication display control through the
  existing H.264/audio/replay pipeline against exclusive mocks. Verify freshness,
  colors, visible border, transitions and overlaps. This is output-scoped capture,
  never an automatic window fallback or proof of game-only isolation.
- [ ] Address the reproduced fixed-region mode-change failure separately: the
  exclusive mock switches this display from 1280x720 to 720x480, invalidating the
  saved 1280x720 crop. Add failing neutral tests, then make invalid crop geometry
  return a non-timeout error rather than endless old-frame reuse. Keep partial
  region boundaries fixed; full-monitor capture is an explicit control/choice.
- [ ] Preserve evidence, document the feasible options and remaining constraints,
  obtain review, run workspace tests (CI=1, no WGC/device tests) and warning-denied
  Clippy for code changes, rebuild/open Clipline, push PR #200 and verify CI.

No WGC, injection, protection bypass, borrowed-handle closes, driver/global runtime
changes, or silent presentation-mode changes. Do not claim a window-only fix if
the successful path captures monitor overlays or requires converting to borderless.

Follow-up: the explicit PrimaryMonitor control also froze. Investigate the DXGI
access-loss lifecycle before treating the region check as sufficient. Microsoft
requires releasing the old duplication interface before creating its replacement;
the existing implementation does the reverse and suppresses creation errors.
- [ ] Record recreation failure during the exclusive transition.
- [ ] Add neutral lifecycle regression coverage, then release stale duplication
  before reopening; preserve bounded retries for temporarily inaccessible desktops.
- [ ] Repeat fixed-region and explicit full-monitor exclusive replay controls.
