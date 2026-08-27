# Screenshot Capture Plan

> Follow the repo's plan-driven TDD convention. Execute task-by-task, write failing tests
> before implementation, and leave checkboxes unticked.

**Goal:** Three hotkey-driven screenshot modes — region, entire screen, active window — that save a
PNG into the media folder, put the image on the clipboard, and show up in the gallery as a new clip
kind. Modelled on ShareX's three capture entries, minus the annotation editor and upload
destinations.

**Exit criterion:** `PrintScreen`, `Ctrl+PrintScreen`, and `Alt+PrintScreen` each write a
`shot_<epoch>.png` beside clips without Snipping Tool also opening, the image is
pasteable into Discord immediately after the keypress, the gallery lists it as a `screenshot` card
that opens a lightbox instead of the review player, storage quota counts it, deleting it leaves no
ghost card, tests/clippy are clean on both CI OSes, and `handoff.md` is updated.

## Design

**Nothing new is needed at the capture layer.** Every primitive already exists:

- `WgcCapture::for_window_client_on` / `for_monitor_on` and `next_frame_timeout`
  (`crates/clipline-capture/src/windows/wgc.rs:159,112,245`) — a still is one session, one frame.
- The session is already `B8G8R8A8UIntNormalized` (`wgc.rs:196`) and already calls
  `SetIsBorderRequired(false)` (`wgc.rs:204`), so there is no format conversion and no yellow
  capture border to flash.
- `read_bgra(&device, &texture) -> BgraReadback { bytes, width, height, stride }`
  (`crates/clipline-capture/src/windows/nv12.rs:433`) does the GPU→CPU readback, and is documented
  as working on WARP and Basic Display Adapter.
- `encode_rgba_png` (`apps/clipline-app/src/game_icon.rs:160`) already encodes PNG via the existing
  `png = "0.17"` dependency, and the BGRA→RGBA swap is at `game_icon.rs:151`.
- `window_from_raw_handle(isize)` (`crates/clipline-capture/src/windows/window.rs:56`) is how the
  app already hands HWNDs across the `windows-sys`/`windows` boundary (`src/service.rs:939`).

So this milestone is orchestration, a region picker, and library integration. **No new dependency.**

**Cursor.** WGC's `IsCursorCaptureEnabled` defaults to true, so a naive still includes the pointer.
Screenshots call `SetIsCursorCaptureEnabled(false)` — the pointer is chrome, not content, and a
region selection would otherwise always capture the crosshair.

**Capture-then-select, not select-then-capture.** The region mode grabs the full monitor *first* and
the overlay shows that frozen frame. Selection is then a crop of pixels already in hand: instant,
and animated content cannot shift under the drag. This is also why region reuses the full-monitor
grab plus a CPU crop instead of the existing GPU `for_monitor_region_on` path — the overlay needs
the whole frame regardless.

**Entire screen means the monitor under the cursor**, not the virtual desktop. WGC has one capture
item per monitor with no virtual-desktop item, so a true all-monitors grab means N sessions stitched
by the `x/y/width/height` offsets from `enumerate_displays()` (`display.rs:19`), plus handling a
non-zero virtual origin and filling the gaps non-aligned monitors leave in the union. Deferred; the
cursor's monitor is what people mean in practice.

**Module layout** follows the platform discipline in `AGENTS.md` — neutral logic testable on Ubuntu,
`unsafe` confined to `windows/`:

- `crates/clipline-capture/src/still.rs` (neutral): `BgraImage`, rect intersection/clamping, the CPU
  crop, and the virtual-desktop union math. Ubuntu CI covers all of it.
- `crates/clipline-capture/src/windows/still.rs`: one-shot grab returning `BgraImage`. Thin wrapper
  over an existing `WgcCapture` plus `read_bgra`.
- `apps/clipline-app/src/screenshot.rs`: resolve target → grab → crop → PNG → atomic publish →
  clipboard → sound → UI event.

**PNG encode and atomic publish are both existing code in the wrong place.** `encode_rgba_png` is
private to `game_icon.rs` and `PosterTemp` (`src/poster.rs:286`) is poster-specific. Both get
promoted to shared helpers rather than copied — the same consolidation
`2026-07-18-l30-shared-windows-helpers.md` and `2026-07-18-l06-poster-temp-ownership.md` already did
for neighbouring code. Screenshots must not invent a second temp-publish story.

**Region overlay is a Tauri window** created in Rust, like the main window (`tauri.conf.json` uses
`create: false`). The frozen frame reaches it through the asset protocol, which already serves a
second media type by exactly this route — `allow_local_poster_asset` grants `&["jpg","jpeg"]` per
file (`library.rs:624`). A `data:` URL is not viable: a 4K PNG is 10–25 MB before base64. Selection
math lives in a pure `ui/region-core.js` and is Boa-tested, matching the precedent set by
`2026-06-12-capture-region.md` ("region math lives in `player-core.js` so it is Boa-tested without a
browser"). The overlay covers the cursor's monitor only, which also sidesteps WebView2 under mixed
per-monitor DPI.

**The library is mp4-shaped and this is the real cost of the milestone.** Every one of these is a
hard filter that silently drops a PNG today:

| Assumption | Location |
|---|---|
| Gallery scan skips non-`mp4` | `library.rs:373` |
| Clip asset grant is `&["mp4"]` | `library.rs:613,621` |
| Quota accounting skips non-`mp4` entirely | `clipline-storage/src/lib.rs:396,432` |
| Clip kind inferred from filename only | `library/naming.rs:3` |
| Duration/track probes assume a movie | `library.rs:1347,1745` |

Screenshots become a `screenshot` clip kind — the discriminator already exists and already drives
card colour (`2026-08-09-purple-clip-kind-contrast.md`), so the gallery, Day grouping, and search
pick them up structurally. What must be *taught to skip* is the movie-shaped work: duration probe,
track counts, audio preview, trim/export, and the review player. Clicking a screenshot opens a
lightbox.

**Cloud upload stays mp4-only in v1.** `upload_mp4_file_with_progress` validates the request against
the file (`cloud_upload.rs:53`), and the create-upload API is movie-shaped. Sharing a screenshot is
its own milestone.

**Hotkeys.** A screenshot needs three new actions, so `HookAction` gains `Region`, `Screen`, and
`Window`, and `HookHotkeys` gains three binding sets — the struct's own comment says adding an
action is "a one-line change at each call site" (`hotkeys.rs:58`). `parse_hook_bindings`
(`hotkeys.rs:318`) already rejects bindings that shadow another action, so Settings surfaces
conflicts for free.

**Defaults match ShareX: `Ctrl+PrintScreen` region, `PrintScreen` entire screen, `Alt+PrintScreen`
active window.** PrintScreen is the key people reach for, so anything else is the wrong default.

This rides the registration path Clipline already has, not the low-level hook.
`is_global_shortcut_hotkey` (`settings/hotkey.rs:12`) routes `HotkeyKey::Function(_)` — modified or
bare — through the Tauri global-shortcut registry, i.e. `RegisterHotKey`, while keyboard and mouse
binds fall to the hook (`app.rs:3203`). `RegisterHotKey` already claims bare F-keys **exclusively**:
`F6` does not reach the focused game today. So PrintScreen joins an existing exclusivity model
rather than changing shared behaviour, and no hook suppression is needed —
`keyboard_proc`'s unconditional `CallNextHookEx` (`hotkeys.rs:437`) stays exactly as it is.

The dependency already supports it: `global-hotkey 0.8.0` maps `Code::PrintScreen → VK_SNAPSHOT`
(`platform_impl/windows/mod.rs:267`) and parses the token `PrintScreen` (`hotkey.rs:295`).

**PrintScreen must be its own `HotkeyKey` variant, not `Keyboard("PrintScreen")`.**
`validate_hotkey_combination` requires `Keyboard` binds to carry Ctrl/Alt/Shift
(`settings/hotkey.rs:118`), so a bare `Keyboard("PrintScreen")` would be rejected by validation
*and* routed to the hook. `HotkeyKey::PrintScreen` follows the `Function(_)` precedent: bare-allowed,
and global-shortcut-routed at every modifier combination.

**The Snipping Tool collision is real but no longer a blocker.** Early-2026 Windows 11 builds added a
"Make Print Screen key yieldable" policy (Computer Configuration → Administrative Templates →
Windows Components → File Explorer) whose *default* — not configured — lets third-party apps take
the key. On a current build, registration should simply succeed. Where it does not (older builds, or
an admin who disabled the policy), the user-scope fix is
`HKCU\Control Panel\Keyboard\PrintScreenKeyForSnippingEnabled = 0` (DWORD, absent means enabled) —
no elevation, and `Win32_System_Registry` is already in the app's `windows-sys` features.

Clipline must **not** silently write that key and call it fixed. Reports are that the change needs an
`explorer.exe` restart to take effect, and ShareX ships exactly this installer option with an open
issue saying it does not work (ShareX#7791). So the honest design is: detect the failure, tell the
user plainly, and offer the toggle as a consent-gated action that says a sign-out or Explorer restart
may be required — the consent pattern from `2026-07-18-l08-plain-http-consent.md`. A failed
PrintScreen registration must also surface in Settings rather than leaving a dead keybind.

The absent-key default follows the `bookmark_hotkey` precedent exactly
(`2026-08-11-user-bookmark-hotkey.md`): a default applies only when the key is *absent* from
settings, is dropped if it collides with an existing binding, and a present-but-blank field stays
unbound.

## Plan-driven implementation

### Task 1: Neutral still-image core

- [ ] Failing tests in `clipline-capture` (run on both CI OSes): rect intersection clamps a
      selection to image bounds; a zero-area or fully-outside rect is rejected rather than panicking;
      cropping honours a `stride > width * 4` readback; BGRA→RGBA conversion is byte-exact; the
      virtual-desktop union computes a non-zero origin for a monitor left of/above primary.
- [ ] Add `crates/clipline-capture/src/still.rs` with `BgraImage`, `crop`, and the union math. No
      `unsafe`, no Windows types.

### Task 2: Shared PNG encode and temp publish

- [ ] Failing tests: `encode_rgba_png` round-trips a known bitmap from its new home; the generalized
      temp-publish helper atomically replaces an existing destination, cleans up on drop when
      unpublished, and reserves unique concurrent temp names.
- [ ] Promote `encode_rgba_png` out of `game_icon.rs` into a shared image module; leave `game_icon`
      calling it.
- [ ] Generalize `PosterTemp` (`poster.rs:286`) into a reusable sibling-temp publisher and port
      poster generation onto it. Behaviour must not change — the existing poster tests stay green.

### Task 3: Windows one-shot grab

- [ ] Failing device tests (self-skipping on CI, per `AGENTS.md`): a monitor grab returns a
      non-empty `BgraImage` matching the monitor's pixel size; a window grab matches the DWM
      extended frame bounds; a destroyed/invalid HWND is a clean `CaptureError`, not a hang; the grab
      times out rather than blocking forever when no frame arrives.
- [ ] Add `crates/clipline-capture/src/windows/still.rs`: `grab_monitor`, `grab_window`, and
      `monitor_at_cursor`, each building a short-lived `WgcCapture`, pulling one frame with a
      timeout, and calling `read_bgra`. Set `SetIsCursorCaptureEnabled(false)`.
- [ ] Confirm the session is torn down on every path, including the timeout — reuse the lifecycle
      discipline from `2026-07-18-m14-windows-capture-lifecycle.md`.

### Task 4: Screenshot orchestration

- [ ] Failing tests: the filename is `shot_<epoch>.png` and survives the reserved-Windows-name and
      normalization checks already applied to clip names (`library/naming.rs`); a screenshot lands in
      the configured media root and is rejected outside it (per
      `2026-07-18-m21-writable-media-root.md`); an unwritable root surfaces an actionable error
      rather than a silent no-op.
- [ ] Add `apps/clipline-app/src/screenshot.rs` with a `ScreenshotMode { Region, Screen, Window }`
      entry point, wired through `RuntimeState` like the bookmark request.
- [ ] Active-window mode resolves `GetForegroundWindow`, refuses Clipline's own window, and reports
      a clear error when WGC declines the target (e.g. an elevated window).

### Task 5: Clipboard image

- [ ] Failing tests: the DIB header is well-formed for a known bitmap (dimensions, 32-bit,
      top-down/negative height, byte length); a failed allocation does not leave the clipboard open.
- [ ] Extend the existing clipboard path in `library.rs:35` to set `CF_DIBV5` (alpha-preserving)
      *and* `CF_HDROP` in one open, so image editors get pixels and Explorer/Discord get the file.
      Reuse the generation-guarded `ClipboardExportState` and the ownership rules from
      `2026-07-18-m18-clipboard-ownership.md` — the clipboard is opened once, filled, and closed.

### Task 6: PrintScreen as a bindable key

- [ ] Failing tests: `PrintScreen`, `PrtSc`, and `PrtScn` tokens all parse to
      `HotkeyKey::PrintScreen`; it is accepted **bare** (no modifier required, unlike
      `HotkeyKey::Keyboard`); `label()` round-trips to `PrintScreen` and `parse_hotkey` yields a
      `Shortcut` for bare and all three modified forms; `is_global_shortcut_hotkey` returns true at
      every modifier combination; the hook's `parse_hook_hotkey` does *not* claim it.
- [ ] Add the `HotkeyKey::PrintScreen` variant, its tokens in `keyboard_key_from_token`'s sibling
      path, `label()`, and the `is_global_shortcut_hotkey` arm.
- [ ] Verify on the dev machine that a registered `PrintScreen` fires Clipline and that Snipping Tool
      does not also open. Record the Windows build number in `handoff.md` — the yieldable-key policy
      default is build-dependent and this is the fact the whole default binding rests on.

### Task 7: Hotkeys and settings

- [ ] Failing tests: the three new bindings dispatch their own `HookAction`s and collide-check
      against save, recording, and bookmark; absent-key defaults are
      `Ctrl+PrintScreen`/`PrintScreen`/`Alt+PrintScreen`; a default that collides with an existing
      binding is dropped; an explicitly null/blank field stays unbound; save/load round-trip;
      `validate()` rejects a screenshot hotkey duplicating another action; `ui_contract.rs` sees the
      new keybind field ids.
- [ ] Add the three `HookAction` variants and `HookHotkeys` fields.
- [ ] Add `screenshot_region_hotkey`, `screenshot_screen_hotkey`, `screenshot_window_hotkey` (plus
      `_secondary` each) to `AppSettings` with load repair, accessors, validation, and `save_to`
      normalization.
- [ ] Settings UI: keybind fields in `index.html` + `settings.js`, with a Screenshots group.

### Task 8: Snipping Tool conflict handling

- [ ] Failing tests: a global-shortcut registration failure for a screenshot bind is reported rather
      than swallowed; the registry probe reads `PrintScreenKeyForSnippingEnabled` and treats an
      absent value as enabled; the consent-gated write sets `0` and is a no-op without consent.
- [ ] Surface a failed PrintScreen registration in Settings next to the field — a dead keybind with
      no explanation is the worst outcome here.
- [ ] Offer the `HKCU\Control Panel\Keyboard\PrintScreenKeyForSnippingEnabled = 0` toggle as an
      explicit, consent-gated action that states a sign-out or Explorer restart may be needed. Never
      write it silently, and never claim success the app cannot verify.

### Task 9: Library integration

- [ ] Failing tests: the gallery scan lists a `.png` as kind `screenshot`; a screenshot entry
      carries no duration and is never handed to the duration or track-count probes; the asset grant
      accepts `png` and still refuses an unlisted extension and a path outside the media root;
      deleting a screenshot removes the PNG and its poster and leaves no ghost card (the regression
      fixed in `0b47bde2`); renaming a screenshot keeps its extension; the "marked" and game filters
      treat a marker-less screenshot sanely.
- [ ] Accept `png` at `library.rs:373`, add a screenshot asset grant beside
      `allow_local_poster_asset`, and return `screenshot` from the kind resolver.
- [ ] Gate the movie-shaped paths — duration, track counts, audio preview, trim/export, review
      player — on clip kind.
- [ ] Gallery card: `screenshot` kind colour, no duration badge, click opens a lightbox.
- [ ] Poster: confirm `ensure_poster` can generate a downscaled thumbnail from a PNG input (a 4K
      screenshot should not be the gallery thumbnail). If ffmpeg-on-PNG misbehaves, downscale in
      Rust with the Task 1 crop/scale code instead of adding an ffmpeg dependency to screenshots.

### Task 10: Storage quota

- [ ] Failing tests in `clipline-storage`: a screenshot's bytes appear in `storage_status`; quota
      pressure and optional auto-delete consider screenshots; a screenshot's sidecar-less layout does
      not confuse recovery.
- [ ] Teach the scan about `png` (`lib.rs:396,432`) and rename the mp4-specific accumulator field to
      something media-neutral. Screenshots are currently invisible to the quota — they would consume
      disk that auto-delete can never reclaim.

### Task 11: Sound and toast

- [ ] Generate `shutter.ogg` (short, quieter than the save sound, distinct from `bookmark.ogg`) with
      the bundled ffmpeg and record the exact command in `handoff.md` so the asset is reproducible.
      rodio is built `default-features = false, features = ["vorbis"]`, so it must be Ogg Vorbis.
- [ ] Add `sound::play_screenshot_taken()` beside `play_bookmark_added()` with the existing
      decode test, plus a UI toast naming the mode and the saved file.

### Task 12: Region overlay

- [ ] Failing Boa tests for `ui/region-core.js`: drag in any direction normalizes to a positive
      rect; the selection clamps to the frozen frame; a click with no drag cancels; Esc cancels; the
      dimension readout matches the physical-pixel rect; snapping to a supplied window rect picks the
      topmost containing candidate.
- [ ] Grab the cursor's monitor, publish the frozen frame to a temp file, grant it through the asset
      protocol, and open a borderless always-on-top overlay window sized to that monitor.
- [ ] Overlay UI: dimmed frozen frame, crosshair, live rect, dimension readout, Esc to cancel.
- [ ] On release, crop the already-captured buffer (Task 1) and continue through the Task 4 publish
      path. Delete the temp frozen frame on both accept and cancel.
- [ ] Measure keypress→overlay-visible. If WebView2 window creation makes it feel sluggish, note the
      number in `handoff.md` and open a follow-up for a native layered window rather than
      hand-rolling one here.

### Task 13: Gates and handoff

- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean, with `cargo clean -p` first for
      changed crates (CI clippy catches lints a warm cache hides).
- [ ] Launch Clipline and verify on the dev machine: each of the three hotkeys during a fullscreen
      game; paste into Discord immediately after; the card appears in the gallery under the right day
      and opens a lightbox; delete leaves no ghost; the quota meter moves; a settings file that
      already binds a PrintScreen combination does not lose it on upgrade; Snipping Tool stays shut
      on all three binds; region select on the non-primary monitor and on a monitor with non-100%
      scaling.
- [ ] Update `handoff.md` and `ddoc.md`.

## Out of scope

- **Annotation/image editor.** ShareX's arrows, blur, and text are a separate product from a
  screenshot key.
- **Upload destinations.** Cloud upload stays mp4-only; sharing screenshots is its own milestone.
- **Virtual-desktop (all-monitors) grab.** Needs N-session stitching; the cursor's monitor covers
  the real use.
- **Group-policy remediation.** If an admin has disabled "Make Print Screen key yieldable", Clipline
  reports it and the user rebinds; the app does not touch machine policy.
- Scrolling capture, GIF capture, OCR, colour picker, and the rest of the ShareX utility belt.
- Window-picker mode (`enumerate_capturable_windows()` exists if it is wanted later).
- Screenshot markers, titles, or metadata sidecars.
