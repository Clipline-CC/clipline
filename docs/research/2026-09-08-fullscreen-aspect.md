# Windows 10 fullscreen aspect preservation

## Result

The explicitly selected full-display DXGI path now preserves source aspect when
fullscreen changes the desktop resolution. A 720x480 source fits a 1280x720
recording as 1080x720 content with 100-pixel black bars on both sides. GPU and CPU
converters share even-pixel placement; crops use their own dimensions, and GPU
crops are revalidated against every new input size. Equal-aspect output is unchanged.

This fixes stretching. It does not establish game-only exclusive capture or
integrate DWM/PrintWindow into production. No WGC, injection, protection bypass,
driver installation, or borrowed-handle close was used.

## Implementation and checks

Neutral tests first reproduced aspect/crop distortion, then passed after fitting.
The GPU video processor explicitly receives source and destination rectangles,
a full-output target rectangle and opaque RGB-black background. A real Radeon
pixel test passed wide/tall/square resize and cropped input: interior Y=235,
padding Y=16 and UV=128 within three code values. Invalid crops after shrink fail.
CPU tests independently check placement, crop shape, padding and existing colors.
Hardware tests self-skip on CI or unavailable video hardware.

Local gates: `CI=1 cargo test --workspace` passed (1,531 tests), followed by
`cargo clippy --workspace --all-targets -- -D warnings` after cleaning the capture
crate. The targeted hardware pixel test was then rerun without CI and passed.

The layout uses integer products with u64 intermediates, checks RECT bounds,
and aligns placement and dimensions for NV12. Rounding costs less than two pixels
per fitted side. An aspect too narrow for one chroma sample fails explicitly.

Microsoft API contracts:
[destination rectangle](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11videocontext-videoprocessorsetstreamdestrect),
[background color](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11videocontext-videoprocessorsetoutputbackgroundcolor),
[output target](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11videocontext-videoprocessorsetoutputtargetrect).

## Physical-machine recording

Windows 10 Home 19045, Radeon 780M, AMD driver 32.0.31041.1004. Existing CABLE
Input stereo endpoint; native H.264 MFT and Opus. Explicit full display
`\\.\DISPLAY3`, Desktop Duplication, 1280x720/60 output. Production auto-detection
was disabled for this display control; this is not a new auto-game validation.

Evidence: `C:\Users\Dain\Desktop\CliplineAspectTest-20260908-141713`.
The flip mock ran 75 seconds with F10 transitions at 15, 20, 35 and 40 seconds;
F6 saved near 70 seconds. All 661 telemetry rows retained fixture foreground.
Main-process private bytes ranged from 91,250,688 to 109,649,920; CPU increased
5.765625 seconds. This short run is not a leak or game-performance certification.

The full session decoded successfully (6,180 video frames, approximately 103.6
seconds including setup/teardown). The replay sample shows correct square geometry,
red/blue references and black bars. After excluding its first two seconds around
the final mode transition, 1,686 decoded frames contain 1,685 counter advances,
zero repeats and zero invalid counter pixels (maximum increment two). These
counters do not prove synchronized or tear-free frames. Sampled live screenshots
show no capture yellow border; display capture does not exclude overlapping apps.

Replay audio/video comparison: 29 pulse edges, video-minus-audio mean -5.812 ms,
range -13.271 to +9.784 ms. Stereo audio remains present. Analysis samples the
new pulse location (430,60), and counter crop is
`crop=528:2:112:164,scale=16:1:flags=neighbor,format=rgb24`.

Media in `C:\Users\Dain\Videos\Clipline\2026-09-08 14-24`:

| File | SHA-256 |
| --- | --- |
| `session_1788891862.mp4` | `AC9F1C3FB2AD3F9F908ECBDA1979D98C14DA08035C3C5AC10C5478C6E7FA7C27` |
| `clip_1788891936.mp4` | `C8306300726F2451C915EC488A29D066619F074D279E5975CB9165D2BCBA22B7` |

## Exclusive classification remains blocked on trace privileges

Update: the user subsequently approved an elevated trace, which completed and
reported hardware-composed independent flip for all 1,201 frames. The remaining
FSO/window-capture comparison needs a separate local UAC acceptance. See the
[presentation trace report](2026-09-08-fullscreen-presentation-trace.md); the account
and initial non-elevated failure below are historical prerequisites, now resolved
for the completed control.

`GetFullscreenState` alone does not distinguish all presentation paths under
[Fullscreen Optimizations](https://devblogs.microsoft.com/directx/demystifying-full-screen-optimizations/).
A PID-scoped [PresentMon](https://github.com/GameTechDev/PresentMon/blob/main/README-ConsoleApplication.md)
trace would distinguish legacy ownership, independent flip and composed modes.

Downloaded the official standalone 2.5.1 x64 release locally. Authenticode is Valid,
Intel Corporation; SHA-256
`9BEC3083069F58F911E6A512F4806DB51A27BD096103087BC1D05EF54C80A191`.
The five-second trace of the mock PID, using a unique session name, failed with
`failed to start trace session: access denied`. The current account is neither
elevated nor a Performance Log Users member. This is a telemetry prerequisite,
not another capture negative. No compatibility setting or group membership changed.

The evidence directory contains a prepared `trace-fullscreen.ps1`: it verifies the
local signature/hash, launches only the mock, and requests UAC for the standalone
PresentMon child. It uses a unique session, a 20-second trace and a ten-second start
delay; no service or driver is installed. It has not been run with elevation.
After trace permission is available, classify the default mock, then compare
disabled Fullscreen Optimizations on a copied fixture only, restore its settings,
and align fresh window-only probe results with sustained PresentMode telemetry.
Success only in composed/independent modes must not be called true-exclusive capture.

League/Valorant installation/login and real-game validation remain outstanding.
