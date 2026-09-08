# Explicit full-display selection and remaining exclusive isolation limit

The Capture target dropdown now distinguishes **Primary display (full)**,
**Full display: [name]**, and **SET REGION**. Selecting a named full display saves
`capture_mode=display_monitor` with a separate `capture_display_id`; its saved
region rectangle is not used to define capture bounds. Desktop Duplication opens
that display in full-monitor mode and follows its source dimensions across mode
changes. This builds on the [fullscreen recovery repair](2026-09-08-exclusive-fullscreen.md).

Legacy primary settings keep primary-display intent. Legacy region settings remain
`display_region`, including rectangles equal to a display's previous dimensions;
opening and saving the dropdown does not infer full-display permission from those
dimensions. An unavailable named full display remains selected in the dropdown,
and capture fails for that identity rather than selecting a different monitor.

This change does not redesign legacy fixed-region recovery: its existing startup
and editor behavior can clamp/rebase regions or recover a missing display. The
new full-display mode bypasses that recovery path. Geometry changes during active
fixed-region capture still stop with the error introduced in the prior repair.

Full-display capture includes visible overlapping windows and can show the desktop
when a game returns to windowed mode. Auto-detected game-window sources are still
rejected by explicit Desktop Duplication before either capture API opens. No
DWM/PrintWindow production integration or WGC fallback was added. Source resizing
still uses the existing fixed encoder output; aspect-ratio preservation remains
a separate issue.

## Strict game-only exclusive capture

The tested DWM and PrintWindow paths still return stale or invalid exclusive-mode
pixels. A review of remaining documented mechanisms found no supported untested
route promising isolation of arbitrary exclusive games under the current no-WGC,
no-injection constraints. This is a bounded feasibility result, not proof that
every private Windows mechanism is impossible.

| Candidate | Why it does not resolve game-only exclusive capture |
| --- | --- |
| DXGI Desktop Duplication | Captures monitor outputs. Foreground checks do not change its source contract. |
| AMD AMF Display Capture | Selects a monitor; presentation-driven timing does not provide window ownership. |
| `D3DKMTGetSharedPrimaryHandle` | Selects an adapter and display source, not an HWND/PID. |
| `DwmDxGetWindowSharedSurface` | Distinct from the probe's user32 export; documented for Windows 7 graphics drivers/runtimes, with surface-update obligations. It is not a supported Windows 10 application capture interface. |

Sources: [Microsoft DXGI](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/desktop-duplication-api),
[AMD AMF](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/blob/master/amf/doc/AMF_Display_Capture_API.md),
[shared primary](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmthk/ns-d3dkmthk-_d3dkmt_getsharedprimaryhandle),
[DWM driver/runtime API](https://learn.microsoft.com/en-us/windows/win32/dwm/dwmdxgetwindowsharedsurface).

A cooperating game could explicitly export a shared render texture. Adding that
to our own mock would only validate cooperation; it would not fix unmodified League
or Valorant. No mock-only shortcut was added or claimed as general game support.
Actual games remain unavailable on this host, and no game-only exclusive fix is
claimed. No new driver installation, injection, protection bypass, surface writes,
or lower-level display-capture prototype was necessary for this conclusion.

## Validation

Neutral tests cover persistence and service routing, required display identity,
independence from saved crop dimensions, primary/region selection preservation,
and rendering/saving an unavailable dropdown selection. Existing capture-policy
tests continue to prohibit a game-window-to-monitor or DXGI-to-WGC fallback.

Local hardware evidence is under
`C:\Users\Dain\Desktop\CliplineFullDisplayTest-20260908-134725`.
The full-display selection was made and saved through the app UI, with the existing
Desktop Duplication backend, native AMD H.264 encoding, stereo VB-CABLE loopback
audio and game auto-detection disabled. The prior settings were backed up locally.

The selected-display flip control ran 75 seconds through two windowed/exclusive
cycles. All 660 telemetry rows kept the fixture foreground. The session and F6
replay decode as H.264 with stereo Opus; sampled live screenshots show no yellow
border, and the session shows fresh content across the mode changes. Excluding
the replay's first two seconds around the final transition, all 1,685 frames in
the steady exclusive tail have valid counters, 1,684 advances and no repeats.
Maximum counter increment was two. This does not establish tear-free frames.

Main-process private bytes after warm-up ranged 84,160,512..106,237,952, and CPU
advanced 5.984375 seconds. Visual/audio pulse comparison over 29 edges averaged
video 10.01 ms ahead of audio (range 4.13..23.70 ms ahead); this is a fixture/device
measurement, not a general synchronization guarantee.

Files under `Videos\Clipline\2026-09-08 13-49`:

- `session_1788889774.mp4`, SHA-256
  `6A57D8C6BFD76B464CC9EFBE687E182C86B75D6ABB87CDC9E812DD9145FA9A51`.
- `clip_1788889849.mp4`, SHA-256
  `23C23F3DD7B51DAE81E516229C04A4823579CDB6871C4466CC07920E7CBCEB61`.

An unavailable-display control uses a deliberately nonexistent persisted ID;
it does not disconnect a real monitor or change a driver. After app restart,
the dropdown shows the selected display as unavailable and Save retains that
identity. The original UI-selected full display is restored after the control.
Explicitly starting capture reports that the selected display was not found and
leaves capture stopped; the local `unavailable-explicit-start.png` preserves it.

Workspace tests with `CI=1` pass (1,525 tests), and warning-denied workspace Clippy
passes after cleaning the changed app crate. WGC/device capture tests are excluded
from that gate to respect the agreed hardware-test scope.
