# Windows 10 native PrintWindow recording experiment

PrintWindow can feed Clipline's real encoder/audio/replay pipeline on this AMD
machine for the native windowed and borderless mocks. This is a standalone example,
**not an app backend or completed automatic game-recording integration**. DWM
shared-surface capture remains unsuitable for the tested flip/exclusive cases.

Host: Windows 10 Home 19045, Ryzen 9 7940HS / Radeon 780M, official AMD driver
32.0.31041.1004. Audio: the existing working VB-CABLE default playback endpoint,
system WASAPI loopback, 48 kHz stereo Opus. No driver changes, reboot, WGC, injection,
protected-window bypass or display fallback were used for these recordings.

## Reproduce

Build and launch one [native mock](../mock-games.md), then use its current HWND/PID:

```powershell
cargo build -p clipline-capture --example print_window_record
.\target\debug\examples\print_window_record.exe --help
.\target\debug\examples\dwm_probe.exe --list
.\target\debug\examples\print_window_record.exe --hwnd 123456 --pid 1234 --seconds 60 --fps 60 --expect-mock --out C:\PrintWindowRecording
```

Replace the HWND/PID and choose a **new** directory. Seconds accepts 3..600, FPS
1..60. Default encoding is the AMD H.264 hardware MFT. `--ffmpeg-amf` explicitly
selects a separate FFmpeg/AMF diagnostic path; it does not enable automatic fallback.
For that option set `CLIPLINE_FFMPEG` to a verified LGPL build. Omit `--expect-mock`
for another intended window; then no automated content-freshness verdict is possible.

Outputs: `session.mp4`, a trailing approximately ten-second GOP-aligned `replay.mp4`,
flushed `samples.csv`, worker stderr and a summary. Failures return nonzero and write
`FAILED.txt`; if recording had started, `partial-finalization.txt` records whether
the valid tail was finalized. A partial MP4 is evidence of failure, not a passing run.
The output directory must not exist, so failed runs cannot overwrite earlier evidence.

## Implementation boundary

Only a child process calls synchronous `PrintWindow` with SDK flag 2
(`PW_RENDERFULLCONTENT`). It uses a reusable top-down GDI DIB, `GdiFlush`, and
client-area cropping. The worker checks the HWND's owning PID, a held process
handle's aliveness, capture affinity, visibility and geometry before and after
each read. Resize/minimize currently stop the fixed-resolution experiment.
[PrintWindow](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-printwindow),
[DIB synchronization](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/nf-wingdi-createdibsection).

One request is outstanding at a time. A versioned, sequenced header validates
dimensions, exact payload length and QPC timestamps before allocating at most
64 MiB. A capacity-one channel delivers the frame with a two-second receive
deadline. Worker transport/protocol failures and deadlines kill/reap the owned
worker before encoder finalization. Parent-side validation failures leave no
capture outstanding; the idle worker is cleaned up after finalizing the valid tail.
A non-inherited kill-on-close job also cleans up the worker if the parent dies.
[Job objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects).

The parent uploads BGRA into a fresh texture on the encoder's D3D11 device, uses
the existing NV12 converter, MFT, `Recorder`, WASAPI and MP4/replay implementation.
The replay ring is bounded to 128 MiB and 12 seconds. Video observation-start QPC
and audio use one `RelativeClock`. PrintWindow has **no presentation timestamp**;
these timestamps are not evidence of synchronized presentation or tear-free frames.
No synthetic duplicate frames are inserted to meet the requested FPS.

## Recorded matrix

All positive rows produced decodable H.264 + stereo Opus session and replay files.
Magenta topmost overlap was added after five seconds and remained visible for the
rest of the overlap runs. Sampled recorded images excluded it and retained the
red/blue/green reference colors. Sampled live desktops showed no yellow border.

| Mock/mode | Requested duration/FPS | Captured frames | Counter advances / repeats | Mean / max PrintWindow ms |
|---|---:|---:|---:|---:|
| Flip windowed, initial | 15 s / 30 | 451 | 450 / 0 | 17.87 / 19.46 |
| Flip windowed, overlap | 60 s / 30 | 1,801 | 1,800 / 0 | 8.44 / 25.45 |
| Flip windowed, overlap | 30 s / 60 | 1,800 | 1,798 / 1 | 13.52 / 29.17 |
| Flip borderless, overlap | 30 s / 30 | 901 | 900 / 0 | 13.25 / 14.73 |
| Flip borderless, overlap | 60 s / 60 | 3,600 | 3,599 / 0 | 11.36 / 24.61 |
| Blt windowed, overlap | 15 s / 30 | 451 | 450 / 0 | 12.77 / 13.75 |
| Blt borderless, overlap | 15 s / 30 | 451 | 450 / 0 | 15.02 / 16.40 |

The strongest run spans 59.9966 seconds between first/last observation starts and
completes in 60.10 seconds including finalization. At 20.7/40.3/60.0 seconds the
recorder's private bytes were 94.56/96.03/96.08 MB (peak 100.50 MB); worker private
bytes stayed near 5.1 MB. Combined recorder/worker CPU time was approximately
28.9 CPU-seconds over the minute, excluding the mock. This is a short steady-state
measurement, not an hours-long leak/performance acceptance test.

Negative controls remain negative:

- Flip DXGI exclusive: preflight returned invalid mock counter pixels, exit 1.
- Blt DXGI exclusive: valid but frozen content, two-second motion stall, exit 1.
- Resize or minimize at five seconds: worker rejected the transition; exit 1;
  earlier 4.8-second session portions finalized and decoded. Restore/recovery is
  not implemented by this prototype.

Exclusive state was checked in the mock's title, which reflects
`GetFullscreenState`; this does not establish the physical scanout path under
Windows fullscreen optimizations. Counter changes cannot establish complete,
synchronized source frames, and these simple D3D11 fixtures are not real games.

## Audio and timing findings

The 60 FPS windowed recording contains left 440 Hz / right 880 Hz tones at about
-17 dBFS during active intervals, alternating with approximately one-second silence.
Measured sinusoid amplitudes: L440 0.199754, L880 0.000040, R440 0.000016,
R880 0.200121. Both tracks decode; loopback is **system audio**, not process-isolated
audio. Interior tone/silence intervals did not show additional >=10 ms gaps.

Decoded visual pulse transitions were compared to `silencedetect` audio edges:

| Recording | Edges | Mean video-minus-audio | Range |
|---|---:|---:|---:|
| PrintWindow windowed 60 FPS | 30 | -82.59 ms | -83.68..-80.94 ms |
| PrintWindow borderless 30 FPS | 30 | -85.36 ms | -86.71..-84.79 ms |
| PrintWindow borderless 60 FPS | 60 | -77.89 ms | -80.72..-73.63 ms |
| Earlier explicit DXGI display/F6 replay | 30 | -33.67 ms | -37.98..-29.35 ms |

Negative means video leads audio. The fixture uses `waveOutGetPosition` to choose
the visual pulse. These measurements combine fixture/virtual-device behavior,
presentation and capture timing. The earlier DXGI baseline is a different run;
it suggests an additional PrintWindow-path lead, but does not isolate its cause.
Do not apply a universal 80 ms offset or declare A/V synchronization solved.

## Separate encoder defect

The explicit FFmpeg/AMF path failed the pipeline's ten-second pending-GOP guard.
A direct 12-second `testsrc2` encode at 800x450/30 with `h264_amf -g 30 -bf 0
-rc cbr -b:v 12000000 -maxrate 12000000 -bufsize 24000000 -usage lowlatency`
also emitted only the initial I-frame. This isolates the observed behavior from
PrintWindow and Clipline's framer. Native AMD H.264 MFT recording succeeded without
changing production encoder code. The FFmpeg behavior remains unresolved; it must
not be worked around by removing the replay safety limit.

## Evidence and next acceptance gates

Local evidence: `C:\Users\Dain\Desktop\CliplineNativePrintTest-20260908-111424`.
It includes target identities/hashes, run/analysis scripts, live/captured images,
CSV counters and process memory, full media decode logs, A/V edge comparisons and
negative controls. Native fixture hash is
`98A0E76A3B8D2918B8C2C14790B2552B7D2F3959A1A950E6376BEE3F9486FF7F`.

Strongest run, `flip-borderless-sixty`, SHA-256:

- `session.mp4`: `04FB39C64131ADBCDF48D2AE8AC4342F4BC6A260EF7888520C52576AE8B79045`
- `replay.mp4`: `910B9BDC1DE80C58B3B3C31FBB2724E20ACC2B81142B595F622B5E5F56488744`

Six neutral tests cover protocol bounds, malformed/truncated packets, timestamps
and mock color/counter decoding. Three Windows tests cover deadline reaping,
blocked-channel cleanup and job close killing its child; none call capture APIs.

Remaining before app integration: explain A/V lead, handle lifecycle/device loss,
establish acceptable long-session overhead and frame integrity, test real engines
and anti-cheat, and decide explicit unsupported-exclusive behavior. League/Valorant
were absent in the machine inventory; installation/login remain prerequisites.
Automatic game detection and global F6 were previously tested only with the app's
explicit display control. The new example does not add those UI behaviors or make
the app's automatic window-capture gate pass.
