# Custom-game window picker with previews

Base: `develop` at `e58395d4`. "Add Custom Game" already lists running windows,
but as text rows (title, exe, PID, path) with no icons, so it reads like a
process list. Make it a screen-share-style picker: a grid of window cards with
a real preview where it can be grabbed cleanly, and the app icon otherwise.

Decision (Dain, 2026-10-07): grab one WGC frame per window only where the
yellow capture border can be hidden (Windows 11, `SetIsBorderRequired(false)`
accepted before capture starts). Older Windows 10, minimized windows and
failed grabs show an icon card. Nothing may ever flash a border.

- [ ] Add failing neutral `clipline-capture` tests (both CI OSes): an Alt-Tab
  style pickability rule over window traits (visible, uncloaked, titled,
  nonzero size; tool and owned windows only with `WS_EX_APPWINDOW`), and a
  BGRA→RGBA area-average downscale that honours row pitch, fits a bounding
  box, keeps aspect, never upscales and forces opaque alpha.
- [ ] Windows: `enumerate_pickable_windows` reuses the existing enumeration
  plus style/cloak/owner/minimized queries and applies the rule. Game
  detection keeps using `enumerate_capturable_windows` unchanged.
  `snapshot_window(hwnd, max)` refuses before `StartCapture` unless the
  border can be hidden, skips minimized windows, disables the cursor, waits
  a bounded time for one frame on a one-buffer free-threaded pool, reads it
  back through a staging texture and closes the session. Device tests
  self-skip on CI.
- [ ] App: `GameWindowInfo` gains the window `handle` and `minimized`;
  `list_game_windows` uses the pickable enumeration (still excluding
  Clipline). New async `window_preview(handle)` re-enumerates, acts only on a
  currently listed non-Clipline handle, snapshots off the main thread and
  returns a PNG data URL (or none). Icons keep using `extract_window_icon`.
- [ ] UI: the dialog becomes a responsive grid of card buttons: a 16:9
  preview area (placeholder → preview or large icon), then icon + title and
  the exe name; the full path is a tooltip. Cards render at once; icons load
  in parallel and previews one at a time, abandoned when the dialog closes
  or refreshes. Selection, dedupe and saving stay as they are. Update UI
  contracts first.
- [ ] Verify in the app on Windows 11: previews for windowed apps and games,
  icon cards for minimized windows, no yellow border at any point, and the
  saved custom game still detects the game.
- [ ] Run workspace tests and fresh-cache warning-denied Clippy on Rust 1.98
  and 1.99, update the handoff, reopen Clipline, and create a PR into develop
  with green OS checks.

Design review (GPT-6.1 Sol) revisions:

- Border gate (blocker): `SetIsBorderRequired(false)` can succeed while
  borderless access is denied and the border still shows. Before
  `StartCapture` require Windows 11, `GraphicsCaptureAccess` borderless
  access returning `Allowed`, and the setter succeeding. The ordering lives
  in a neutral helper whose tests assert zero starts on every denial. The
  guarantee covers Clipline's session only.
- Recycled handles: previews take the listed handle and PID; the worker
  checks the window still belongs to that PID before creating the capture
  item and again after the frame, returning nothing on mismatch.
- Icons: `extract_window_icon` moves off the main thread onto a worker with
  COM initialised; the UI shares one icon request per PID per scan.
- Owned windows whose owner is hidden stay listed (gameplay windows owned by
  a hidden bootstrap window); owned windows with a visible owner do not.
- Geometry: crop to the frame's positive `ContentSize`, reject content larger
  than the surface. Windows on an HDR display get an icon (BGRA8 would wash
  out). Budget ~750 ms per preview including readback; one picker-owned
  device behind a mutex, so at most one native capture at a time.
- UI: a DOM-free `window-picker-core.js` (tested through boa like the other
  cores) owns one preview queue across refreshes: one request in flight,
  stale results dropped after refresh, close, reopen or selection, and no
  further requests once abandoned. Selection still works without a preview
  and still saves the executable icon.

Plan checkboxes remain unticked by repository convention.
