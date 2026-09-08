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

## Window-only comparison preparation

A byte-identical copy named `CliplineMockFlipFsoControl.exe` was placed in the
aspect evidence directory. Its Properties > Compatibility page showed the Disable
fullscreen optimizations checkbox unchecked. Its per-user compatibility value
was absent, recorded in `fso-before.json`. No compatibility setting has changed yet.

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
The corrected control still requires acceptance of the local Windows UAC prompt.

Next: finish the default copied-fixture control, change only that copy's Disable
fullscreen optimizations checkbox, relaunch and repeat, then restore the setting.
Group PresentMode by swapchain during the actual probe interval. A checkbox alone
is not proof of a presentation change. If both controls fail, or the toggle changes
neither mode nor capture freshness, end this FSO/PrintWindow branch without claiming
that all possible game-only APIs are impossible. No production integration follows
from these diagnostics.
