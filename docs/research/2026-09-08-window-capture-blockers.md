# Windows 10 capture blocker investigation

Continues the [native mock matrix](2026-09-08-mock-game-validation.md) on Windows
10 Home 19045 / Radeon 780M / AMD driver 32.0.31041.1004. Production recording
remains unchanged: no WGC, injection, protected-window bypass or DWM integration.

## Surface diagnosis

The DWM probe reacquires the handle and reopens the texture on every sample,
selects the producer adapter, and copies/maps matching staging resources. Review
found no caching/copy defect explaining why blt works and flip fails.
Microsoft documents that blt copies into a redirection surface while flip shares
back buffers directly with DWM. The observed private export appears to expose
that older redirection content; this is an inference, not an undocumented API
contract. [DXGI flip model](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/dxgi-flip-model).

`DwmFlush` is not a supported way to update another application's unused
redirection surface. Its documented scope is queued drawing by the caller.
[DwmFlush](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/nf-dwmapi-dwmflush).
The similarly named `DwmDxGetWindowSharedSurface` is a different driver/runtime
interface, not an application capture replacement.
[Microsoft's driver API restriction](https://learn.microsoft.com/en-us/windows/win32/dwm/dwmdxgetwindowsharedsurface).

## PrintWindow results

`PW_RENDERFULLCONTENT` is defined as `0x2` by the installed Microsoft Windows SDK
`10.0.26100.0/um/WinUser.h`. Its GPU capture behavior is not guaranteed by the
public PrintWindow page. PrintWindow is synchronous, and target-dependent; it must
not run on Clipline's UI/recorder thread without process isolation.
[PrintWindow documentation](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-printwindow).

| Test | Result |
|---|---|
| Flip windowed, flags 0 | White client area despite successful API return |
| Flip windowed, flags 2 | Correct moving content and colors |
| Flip overlapped by Clipline, flags 2 | Fresh content; Clipline excluded |
| Flip borderless, flags 2 | Correct moving content and colors |
| Flip DXGI exclusive, flags 2 | Frozen pre-transition content or blank output despite success |
| Blt windowed/borderless, flags 2 | Correct motion and reference colors |
| Blt DXGI exclusive, flags 2 | Frozen content: 107 valid counters, zero counter changes |
| DWM after PrintWindow calls | Still static: 148 reads, zero changes in five seconds |

The exclusive test verified the fixture's DXGI state, but not physical scanout
under Windows fullscreen optimizations. Three exclusive BMPs were byte-identical
while the live title advanced from frame 4560 to 4650. Thus switching APIs does
not resolve exclusive capture. The mock was not modified to implement WM_PRINT.

A 60-second overlapped flip run produced 1,292 reads, 1,292 valid binary counters,
1,291 counter changes, zero errors. Calls averaged 10.90 ms (maximum 19.52 ms).
The entire PowerShell harness achieved about 21.5 samples/s at requested 30 FPS;
these timings are not a native recorder throughput guarantee. Private bytes were
80.5 MB at the first sample, 79.7 MB at the last, with a 123.4 MB peak; this short
run does not establish long-term stability. Its supervisor initially lost the
worker exit code through Windows PowerShell `Start-Process -PassThru`; the evidence
was valid but the wrapper reported failure. Direct ownership of the child process
handle fixes that reproduced harness defect.

The reviewed harness also validates red/blue reference anchors before decoding
the binary counter, rejects invalid counter pixels, and fails if counter progress
stops for more than two seconds. A subsequent 10-second overlapped run passed:
219 reads, 218 counter changes, zero invalid counters/errors. Flags 0 and exclusive
negative controls correctly fail with preserved evidence. This checks sampled
progress, not tear-free output or precision synchronization.

A subsequent 15-second visible flip run passed 345 valid counters / 344 changes,
zero invalid counters/errors and no >2s progress gap. Its live desktop screenshot
showed no yellow capture border. This is a sampled observation; extended border
and real-game acceptance remain outstanding.

## Reusable diagnostic

Run one of the registered [mock executables](../mock-games.md), obtain its HWND and
PID from `dwm_probe --list` / the running process, and choose a new directory:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-print-window.ps1 -Hwnd 123456 -ExpectedProcessId 1234 -OutputDirectory C:\PrintWindowTest -Seconds 60 -Fps 30 -ExpectMockMotion
```

The example HWND/PID must be replaced. The wrapper runs a worker with a hard
duration-plus-15-second timeout, kills only that worker if it stalls, and preserves
stdout/stderr/exit status and flushed sample data. Every read validates source
identity and capture affinity, including after the blocking call. It never falls
back to a display. Snapshots can contain window content and stay local.

`-Flags 0` is a negative comparison; default is 2. `-ExpectMockMotion` requires a
Clipline mock executable and its known pixel pattern; omit it for other intended
windows and inspect actual content manually. It rejects a planned minimize/pause
longer than two seconds as well, so interpret such runs by their timeline. Neither
mode alone establishes that a complete game recording is possible.

## Thumbnail control and DWM acceptance fix

A documented DWM thumbnail of the flip source displayed fresh motion in an owned
magenta-background helper window. Reading that helper through the original DWM
probe returned only magenta: 148 reads and zero changes in five seconds. The
public thumbnail API renders into a destination window; it does not provide a
readable capture texture. No desktop readback was substituted into recording.
[DWM thumbnails](https://learn.microsoft.com/en-us/windows/win32/dwm/thumbnail-ovw).

The DWM probe now offers `--require-motion`, also enabled by its controlled-target
runner. It fails an all-identical animated-target run after saving BMP/CSV/summary
evidence. Neutral tests cover the opt-in parser, duplicate flag, static sampling,
no-readable-frame and changing-frame cases. The hardware flip negative control
returns exit 1 for 148 unchanged reads; this fixes misleading automated acceptance,
not the unavailable flip surface.

The blt positive control returns exit 0 for 118 reads / 117 pixel changes.
Workspace tests pass with `CI=1` to skip WGC/device tests; a fresh capture-crate
Clippy pass with warnings denied also passes. Supervision tests cover success and
failure exit codes, timeout, preserved diagnostics and paths with spaces, without
calling capture APIs. The Windows CI job runs these tests. A workspace format check
reported existing differences across unrelated files; only the edited probe files
were formatted. Clipline was rebuilt and reopened after the checks.

## Remaining integration work

Update: the [native recording experiment](2026-09-08-print-window-recording.md)
now exercises process isolation, frame delivery, GPU upload, audio and replay,
including a successful 60-second 720p60 borderless mock run. It also exposes an
A/V lead and preserves exclusive/transition failures. The list below describes
what remained at this earlier screenshot-only checkpoint.

PrintWindow is a promising experimental path for ordinary windowed/borderless
capture. It still needs native process isolation, bounded frame delivery, CPU to
GPU transfer and timestamp handling, resize/minimize/device-loss recovery, border
observation during sustained capture, real-game compatibility and end-to-end
audio/replay testing. Exclusive fullscreen remains unsupported by both tested
window-only readback paths. Do not silently switch to desktop capture or change
a game's presentation mode to label this fixed.

Evidence: `C:\Users\Dain\Desktop\CliplineWindowResearch-20260908-045948`. Use
`thumbnail-live-confirmed` for the live-thumbnail control; earlier attempts had a
zero-size marshaled rectangle or expired target and are not valid comparisons.
Raw screenshots, helper scripts and sample logs remain local.
