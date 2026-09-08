# Capture API audit after native-exclusive failures

The [classified DWM/thumbnail experiment](2026-09-08-dwm-native-exclusive-followup.md)
returned static or stale content while the source continued presenting through
Legacy Flip. Further API review has not identified a supported replacement that
isolates an unmodified native-exclusive game on this Windows 10 machine without
injection. This is a feasibility limit of the mechanisms reviewed, not a proof
that every private Windows mechanism is impossible.

## Local capability check

On Windows 10 build 19045, queried WinRT `ApiInformation` only. No capture session
was constructed and no permission request or system setting change was made.

| Metadata query | Result |
| --- | --- |
| `Windows.Graphics.Capture.GraphicsCaptureSession` type | Present |
| `GraphicsCaptureSession.IsBorderRequired` property | Absent |
| `Windows.Graphics.Capture.GraphicsCaptureAccess` type | Absent |
| `Windows.Media.AppRecording.AppRecordingManager` type | Present |

Evidence: `C:\Users\Dain\Desktop\CliplineCaptureApiAudit-20260908-160808\api-presence.json`.
This directly verifies that the documented border-control option is unavailable
on this host. Microsoft's [property contract](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired)
starts at build 20348 and requires consent for borderless capture. A newer SDK
does not add that OS implementation to build 19045. WGC was not invoked.

## Game DVR is not an arbitrary-window capture API

The installed Game Bar and available AppRecordingManager type are insufficient
to establish a Clipline backend. Microsoft's [AppRecordingManager contract](https://learn.microsoft.com/en-us/uwp/api/windows.media.apprecording.apprecordingmanager)
captures the calling UWP app's content. Its documented recording, historical
recording and screenshot methods do not accept an HWND or PID. The installed
Windows SDK 10.0.26100.0 `winrt/windows.media.apprecording.idl` confirms the
public manager's methods and parameter lists. Modifying our own mock to call
these APIs would test a cooperating application, not capture an unmodified game.

The [Game Bar widget API overview](https://learn.microsoft.com/en-us/xbox/game-bar/api/api-overview)
documents widget lifecycle, target tracking, notifications and hotkeys; it does
not expose a target application's frame stream. Microsoft's
[WDDM 2.1 GameDVR feature](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/wddm-2-1-features)
describes driver support for fullscreen Game Bar performance through present
batching. It does not supply an application-level isolated capture interface.
No private Game DVR service was invoked and no Game Bar settings were changed.

## Source selection remains the decisive distinction

The [previous API review](2026-09-08-full-display-selection.md#strict-game-only-exclusive-capture)
still applies: DXGI duplication, AMD AMF Display Capture and the shared-primary
kernel interface select display outputs. Checking foreground ownership before
and after a read cannot atomically establish which process produced every pixel
in that read. Cropping or stopping after focus loss does not turn these APIs into
an isolated window source.

Discord's [published capture explanation](https://support.discord.com/hc/en-us/articles/9410427556375--Windows-Capturing-Application-Window-for-Screen-Share-and-Go-Live)
describes its signed injected capture DLL and states that WGC does not work for
full-screen exclusive games. This is context about that implementation, not a
test of the current Discord build or proof about every capture implementation.
No third-party capture code was read or copied for this audit.

## Concrete paths and acceptance limits

| Direction | What it preserves | What must change or remains unproven |
| --- | --- | --- |
| Continue PrintWindow recording in windowed/borderless mode | Experimental window source, no injection, sampled absence of border | Game must use that mode; resize/minimize recovery, timing, auto-start and real games still need validation |
| Explicit full-display recording for exclusive games | Border-free DXGI with existing audio and replay pipeline | Display content is allowed in the recording; concurrent native-exclusive tracing of this recorder is still needed |
| Keep native-exclusive, game-only and no-injection requirements | Original strict source contract | No passing mechanism has been found; production integration cannot honestly be called a fix |

Changing game presentation or capture source is a product decision, not a silent
fallback. No such change is made by this audit. Injection remains excluded.
Clipline remains open with capture paused; the existing experimental mechanisms
remain separate from production.
