# Windows 10 fullscreen presentation trace

## Completed control

With user authorization, the official Intel-signed PresentMon 2.5.1 standalone
executable completed a PID-scoped, elevated 20-second trace. No service, driver,
runtime installation or account-group change was involved. The unique ETW session
and PresentMon processes ended after the trace.

Evidence directory:
`C:\Users\Dain\Desktop\CliplineAspectTest-20260908-141713\fullscreen-classification-20260908-145421`.
The binary hash and signer are recorded in the [aspect report](2026-09-08-fullscreen-aspect.md).

| Item | Observation |
| --- | --- |
| Fixture | Original `games-v2\CliplineMockFlip.exe`, PID 15952 |
| Adapter | Radeon 780M, AMD driver 32.0.31041.1004, Windows 10 Home 19045 |
| Requested mode | F10 / `SetFullscreenState(true)`; title subsequently reports exclusive |
| PresentMon rows | 1,201, all the intended PID and swapchain `0x1A3FE85CDE0` |
| Trace interval | TimeInMs 10044.0057 through 29939.0123 |
| Every PresentMode | `Hardware Composed: Independent Flip` |
| Capture during trace | No new recording/probe; Clipline was paused with a replay open |

PresentMon defines this mode as a flip-model swapchain granted a hardware overlay
plane. It is distinct from the Legacy Flip/Legacy Copy modes that report screen
ownership. See the [official PresentMode definitions](https://github.com/GameTechDev/PresentMon/blob/main/README-ConsoleApplication.md).
Windows can report fullscreen to an application while using Fullscreen Optimizations;
see [Microsoft's explanation](https://devblogs.microsoft.com/directx/demystifying-full-screen-optimizations/).

This trace does **not** demonstrate native-exclusive screen ownership. Earlier
mock runs labeled exclusive established the application's requested/reported mode,
not a measured presentation classification. This later trace cannot retroactively
assign a PresentMode to those earlier recordings. Their observed recovery and
capture-pixel results remain valid within that qualification.

## Completed window-only comparison

A byte-identical copy named `CliplineMockFlipFsoControl.exe` was placed in the
aspect evidence directory. Its Properties > Compatibility page showed the Disable
fullscreen optimizations checkbox unchecked. Its per-user compatibility value
was absent, recorded in `fso-before.json`. The same copy/hash was relaunched for
each control. Clipline was stopped, and only the signed PresentMon child was
elevated; the fixture and PrintWindow probe remained unelevated.

| Control | FSO enabled | FSO disabled |
| --- | --- | --- |
| Evidence subdirectory | `fso-enabled-window-control-20260908-145951` | `fso-disabled-visible-20260908-151324` |
| Fixture PID | 716 | 16284 |
| PresentMon frames | 1,201 | 1,201 |
| Every PresentMode | Hardware Composed: Independent Flip | Hardware: Legacy Flip |
| Swapchain | `0x22BA9A35150` | `0x1E48AC0E3E0` |
| Events during probe startup/failure interval | 21 | 8 |
| PrintWindow flags=2, fresh DIB, mock counter validation | Immediate invalid counter pixel | Immediate invalid counter pixel, exit 1 |
| Probe interval | About 343 ms | About 135 ms |
| Foreground telemetry | All 252 rows retained fixture foreground | All 254 rows retained fixture foreground |

Each trace was bounded to 20 seconds; each probe requested 15 seconds but failed
on its initial counter validation before producing usable recording output. These
are failed initial reads, not 15-second freshness measurements. The enabled
helper did not retain the process exit code (null); its stderr and `FAILED.txt`
independently record the failure. The disabled helper retained exit code 1.

The disabled control **does establish legacy exclusive presentation**, including
the probe interval, unlike the hardware-overlay control. Toggling FSO changed
presentation but did not repair PrintWindow. This ends the tested FSO/PrintWindow
branch without a production capture change. It does not prove all possible
game-only capture APIs impossible, and it does not add a classified DWM or monitor
capture result. Earlier untraced DWM/display runs retain their original limits.

### Timestamp alignment

The enabled control used PresentMon `--date_time`. Its CSV wall clock was four
hours behind Windows local time (eight behind UTC). Tagged
[PresentMon 2.5.1 PMTraceSession](https://github.com/GameTechDev/PresentMon/blob/v2.5.1/PresentData/PresentMonTraceSession.cpp)
converts the live start FILETIME to local time, then applies local conversion
again in `TimestampToLocalSystemTime`. The machine's current offset was UTC-4;
adding eight hours to this CSV aligns its events with the independently logged
UTC probe interval, 19:06:56.1078421 through 19:06:56.4512223. The constant offset
correction is specific to this run/date; it is not a general timestamp parser.

The disabled control instead used `--qpc_time`, directly compared with
`Stopwatch.GetTimestamp()` at frequency 10,000,000. Eight events fall within
QPC 478230998672 through 478232347989; all report Legacy Flip. No timezone
conversion is involved. The event window includes process startup and the first
observed exit, rather than precise entry/exit timestamps of the PrintWindow call.

CSV SHA-256:

| Control | SHA-256 |
| --- | --- |
| FSO enabled | `03E56B7A89A35D4AE29C9AE8CB7214C1F8046A5CB91C4E9A90B00EC04AFE2005` |
| FSO disabled | `119A1FFF4EAB7B7E9E765E0B263C545588D94BC14A1833DFEE890E661DAF51EE` |

### Elevation, cleanup and excluded attempts

The copied executable's Disable fullscreen optimizations checkbox was changed
through Properties and the resulting per-user value was recorded. The final
helper reapplied only that observed value to the same copy, then restored its
original absent value in `finally`. `fso-restored.json` and an independent registry
read confirm restoration after the completed disabled control. The original mock
and global/account settings were untouched. PresentMon and fixture processes exited.

The first combined PresentMon/PrintWindow attempt stopped before recording because
the UAC prompt outlasted the helper's fixed seven-second delay. Its empty output
directory, `fso-enabled-window-control-20260908-145827`, is an aborted setup attempt,
not capture evidence. The initiating PresentMon process was stopped. The attempted
desktop screenshot during UAC failed with an invalid handle; its black image is
not a captured game frame.

The local `trace-window-control.ps1` helper was corrected to wait for UAC consent
using ShellExecute (`Start-Process -Verb RunAs`) before continuing. It elevates only
the signature/hash-verified PresentMon child. The fixture and PrintWindow probe
stay unelevated. It restores foreground, requests fullscreen, verifies the title
and foreground, then starts a fresh flags=2/fresh-DIB PrintWindow probe for 15
seconds alongside a 20-second timestamped PresentMon trace of that fixture PID.
Another disabled-control elevation attempt (`fso-disabled-window-control-20260908-150837`)
returned Windows cancellation before capture; its fixture exited and a scoped
cleanup helper restored the copied executable's setting. It is also excluded.
The final retry used a visible helper, verified that Windows consent was active,
and proceeded only after the user accepted it. The final helper records explicit
status, preserves the child process handle for its exit code, uses QPC alignment,
and restores the compatibility value even on failure.

No Clipline production code changed for this comparison. The existing source
quality gates remain those of `d9bc8e8`; documentation changes were checked with
`git diff --check`, independently reviewed, and published to PR #200.
