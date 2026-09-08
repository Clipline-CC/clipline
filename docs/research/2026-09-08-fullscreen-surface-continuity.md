# Window capture across fullscreen at unchanged resolution

The user clarified that the goal remains making the experimental window capture
work in fullscreen. This control isolates display resizing from the presentation
transition in the earlier [native-exclusive test](2026-09-08-dwm-native-exclusive-followup.md).

## Result

At constant 1280x720, PrintWindow flags=2 captures fresh game content in borderless
mode, freezes at counter 5552 during measured native-exclusive presentation, and
captures fresh content again after returning to borderless. Direct DWM remains
static in all three phases. The exclusive failure is therefore not explained
solely by the earlier 1280x720-to-720x480 display mode change.

This supports presentation-dependent availability of the content these APIs read.
It is not proof about the internals of an undocumented export, nor a general
impossibility result. No capture implementation defect or passing exclusive fix
was identified by this control.

## Setup and source continuity

Same Windows 10 Home 19045 / Radeon 780M / AMD driver 32.0.31041.1004 host.
The unchanged original flip mock was copied into a new directory as
`CliplineMockFlip.exe`; SHA-256 remained
`98A0E76A3B8D2918B8C2C14790B2552B7D2F3959A1A950E6376BEE3F9486FF7F`.
Only that copy's per-user Disable fullscreen optimizations value was temporarily
set. Its rendering, swapchain code and message handling were unchanged.

F11 established borderless 1280x720 before F10 entered exclusive; a second F10
returned to borderless. All 300 source telemetry rows retained foreground and
reported client and monitor dimensions of 1280x720. The same process/window
survived the entire test. Existing probes ran in fresh, bounded child processes
against that source; PrintWindow retained identity and affinity checks.

Microsoft [requires a buffer resize after flip fullscreen transitions](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiswapchain-setfullscreenstate).
The existing fixture already performs that operation. This control changes the
dimensions before entering exclusive through its existing hotkeys, rather than
adding a capture-specific cooperating implementation.

## Concurrent presentation and pixel results

User-approved, verified Intel-signed PresentMon 2.5.1 ran a PID-scoped 45-second
trace with QPC timestamps. It produced 2,682 events with zero wrong-process rows.
Only PresentMon was elevated. The fixture and capture probes remained unelevated.

| Phase/probe | Capture result | Concurrent PresentMon events |
| --- | --- | --- |
| Before, PrintWindow | 51 valid reads, 50 counter advances, 5122 to 5357, exit 0 | 261 hardware-composed independent flip |
| Before, DWM | Motion-required failure, exit 1 | 182 hardware-composed independent flip, 1 composed flip |
| Exclusive, PrintWindow | 51 valid reads, zero advances, all counter 5552, exit 1 | 262 Legacy Flip |
| Exclusive, DWM | 88 reads, zero changes/errors, exit 1 | 183 Legacy Flip |
| After, PrintWindow | 51 valid reads, 50 advances, 6245 to 6480, exit 0 | 259 hardware-composed independent flip |
| After, DWM | Motion-required failure, exit 1 | 183 hardware-composed independent flip |

During exclusive PrintWindow sampling, live source titles advanced from frame
5670 to 5925. Thus counter 5552 is stale, rather than a stationary game. All 51
counter samples are valid under the unchanged geometry, with no API errors or
invalid-counter results. First/middle/last BMPs are byte-identical. The inspected
image shows the correct game layout and colors but outdated content.

Exclusive DWM ran for 3.013 seconds and returned unchanged white/black content.
Its mean read time was 5.749 ms; PrintWindow's was 11.425 ms. These short readback
timings are not recorder throughput or stability guarantees. No audio, encoder,
replay or new border-acceptance result is claimed for this control.

## Evidence and cleanup

Root: `C:\Users\Dain\Desktop\CliplineFullscreenContinuity-20260908-180642`.
Completed run: `run-20260908-180819`. Local artifacts include the bounded
`run-continuity.ps1` harness, per-stage samples/BMPs, `fixture.csv`, `stages.json`,
`classification-summary.json`, `counter-summary.json` and snapshot hashes.

| Artifact | SHA-256 |
| --- | --- |
| PresentMon CSV | `8398CD048FD938410CB1B17A98CDAB535F5E3D32A4476BE0E8D8EF049CFBF786` |
| Each exclusive PrintWindow BMP | `5DF91CD5926A9CA4299C790D55D6A875BBA062EC280663D858D1A816AA150C8F` |
| Each exclusive DWM BMP | `598EB6A5B7E1D3291E1E195CA43891F66990FCFB3307CC205D208D4C9CAA8CAB` |

The copied fixture's compatibility value was restored to absent in cleanup and
independently verified afterward. Fixture and trace processes exited. No WGC,
monitor fallback, injection, protection bypass or production backend change was
introduced. Borrowed DWM handles remain unclosed. Clipline remains paused.
