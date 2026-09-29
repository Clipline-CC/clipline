# Windows 10 native mock-game validation

Machine: Windows 10 Home 19045, Radeon 780M, official AMD driver
`32.0.31041.1004`. CABLE Input is the working Windows default playback device and
Clipline output-capture endpoint. Microphone/process splitting are disabled.
Clipline uses explicit Desktop Duplication and AMD AMF H.264.

The [standalone mock games](../mock-games.md) use hardware D3D11 with blt or flip
presentation, motion/frame markers and generated stereo audio. Both were built
with MSVC `/W4 /WX /MT` and registered through Clipline's running-window picker.
The UI identified each executable and displayed its changing window title.
Closing/relaunching the blt mock and switching to the flip mock exercised detection.
Starting the replay buffer for either detected window showed the explicit
Desktop Duplication window-source error. No automatic game-only recording was
produced; full-session auto-capture acceptance remains blocked at that same source
boundary. No WGC diagnostic or production DWM integration was performed.

## DWM presentation matrix

Ten-second runs at requested 60 reads/s, against explicit HWNDs. The fixture was
raised visibly, the requested mode was verified from its DXGI-backed title, and
BMPs were inspected. Fullscreen state is the DXGI API state, not independently
verified physical exclusive scanout under Windows fullscreen optimizations.

| Presentation | Mode | Reads | Changed samples | Result |
|---|---|---:|---:|---|
| Blt / DISCARD | Windowed | 583 | 581 | Correct colors and moving content |
| Blt / DISCARD | Borderless | 581 | 578 | Correct colors and moving content |
| Blt / DISCARD | DXGI exclusive | 581 | 0 | Frozen, fragmented pre-transition content |
| Flip / FLIP_DISCARD | Windowed | 583 | 0 | White surface while live scene renders |
| Flip / FLIP_DISCARD | Borderless | 582 | 0 | Static white/black stale geometry |
| Flip / FLIP_DISCARD | DXGI exclusive | 581 | 0 | Static white/black stale geometry |

Each run reported zero read errors and about 58 reads/s. The blt live title advanced
from frame 105 to 705 in the exclusive run while captured samples stayed identical.
A separate 15-second blt windowed run behind the maximized Clipline window produced
874 reads, 869 changed samples, no errors; the captured BMP excluded Clipline and
retained the correct red/blue/green blocks. Thus a successful surface read or a
zero exit code does not imply useful capture. The probe emits a freshness warning
for unchanged samples but its process can still exit zero.

No yellow border was observed in the sampled live fixture views. These findings
reproduce the accelerated-browser failure with a native executable and narrow it
to presentation-dependent behavior on this host. They do not establish a universal
cause, tear-free frames or support for real engines, anti-cheat, HDR, device loss
or sustained game recording. Resize/minimize recovery was previously exercised
with the GDI fixture; it was not repeated for this native six-mode matrix.

## Evidence and reproduction limits

An explicit display-control replay of the borderless flip fixture, with automatic
game switching disabled, saved and played in Clipline. The first version's four
10 ms waveOut buffers reproduced short audio gaps and a playback clock that fell
behind elapsed time. Increasing them to four 100 ms buffers removed those observed
gaps. This is a fixture fix; no audio-recorder code changed.

The corrected `clip_1788857136.mp4` is 19,107,131 bytes, 30.00 seconds, 1280x720
60 fps H.264 plus 48 kHz stereo Opus. Complete decoding succeeds with 1,800 frames.
Silence detection finds 15 intended gaps of 0.993-0.995 seconds and no additional
gaps of at least 10 ms at the -45 dB threshold. A 1.6-2.1 second tone sample has
channel RMS -17.025/-17.025 dBFS and zero-crossing rates 0.018371/0.036663, consistent
with 440 Hz left / 880 Hz right. SHA-256:
`99FF9DF168B474A7E9D0D1D3640466BF65FFB2119FF91C2CE69D1241A65F14B6`.
That replay was saved using the app's Save button. A separate F6 repeat, after
confirming the buffer was ready and the mock was the foreground HWND, also saved
successfully: `clip_1788857364.mp4`, 19,107,106 bytes, 30 seconds / 1,800 decoded
frames with stereo Opus. SHA-256:
`0B6035CF7FB4D05B6A729713DFECAD7D28E8159A490A12E1DEABE549BDB4CCED`.
Earlier synthetic F6 attempts had inconclusive capture state and should not be
treated as a reproduced hotkey defect.
This control records the display, including UI exposed around the save action;
it is not proof of game-only capture, automatic start or tight A/V synchronization.

Final fixture SHA-256:
`98A0E76A3B8D2918B8C2C14790B2552B7D2F3959A1A950E6376BEE3F9486FF7F`.
Both registered executable paths were updated to this build. Clipline is left
open with capture paused, automatic switching off, both custom games registered,
and CABLE Input selected. The corrected replay is available in its library.

Local evidence root: `C:\Users\Dain\Desktop\CliplineMockGameTest-20260908-042205`.
The `suite-*` directories contain BMPs/CSV/summaries; title files and screenshots
establish requested modes and live rendering. Raw desktop screenshots remain local.
One earlier directory named `dwm-flip-borderless` came from a failed foreground
interaction and is not evidence of borderless capture. Use the verified `suite-*`
matrix instead. PowerShell's stderr handling interrupted some early harness
commands after the probe's freshness warning; the completed BMP/summary files
were inspected directly.

Matrix binary SHA-256: `094DD9D521A0C76182D75525DBDD968A29F1C39DD3CBCA7D3B8478D21B99F08C`.
Later fixture revisions fix error-path HWND lifetime and increase audio buffering;
these are not changes to the probe or production capture code.

Workspace tests pass with `CI=1` (device tests skipped to avoid starting WGC), and
warning-denied workspace Clippy passes. The initial test command hit the running
app's executable lock; stopping Clipline and rerunning passed. Code review caught
and fixed an exception-path window/userdata lifetime bug. Clipline was rebuilt
and reopened. During test setup a PowerShell UTF-8 BOM caused settings recovery;
the intended saved settings were restored as BOM-free UTF-8, including both mock
registrations and the explicit DXGI/audio selection. This was a harness mistake.
