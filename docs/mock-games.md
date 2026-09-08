# Windows mock games

These first-party standalone executables exercise executable-based game detection
with hardware D3D11 animation and real output audio. They do not call capture APIs,
inject into another process, or change Clipline's capture backend.

With Visual Studio C++ Build Tools and a Windows SDK installed:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-mock-games.ps1 -OutputDirectory C:\CliplineMockGames
C:\CliplineMockGames\CliplineMockBlt.exe --seconds 180
C:\CliplineMockGames\CliplineMockFlip.exe --seconds 180
```

The output directory must be new. Use ASCII build paths without batch
metacharacters. MSVC builds with `/W4 /WX /MT`; this small C++ fixture has no Opus
dependency and does not solve the separate Rust probe's static-CRT linker issue.
Both executables contain identical code; their basenames select blt (`DISCARD`)
or flip (`FLIP_DISCARD`) presentation. `--blt` / `--flip` override that selection.
Hardware D3D11 and a working default stereo playback endpoint are required.

In Clipline, open **Settings > Games > Add Custom Game**, select the running
mock, then save. Register both executables, running only one at a time for audio
tests. Windows' default playback device should match Clipline's output capture
device; on the PiKVM host this is the working **CABLE Input** virtual endpoint.

For automatic-capture acceptance, enable automatic game switching and games-only
waiting, save, and arm the replay buffer. Close, launch and relaunch a registered
mock. Verify detection, buffer start, F6 replay saving and waiting after exit.
Also select the mock's full-session mode for a separate start/stop test. Decode
the saved media and inspect it in Clipline's player.

**Current Windows 10 blocker:** explicit Desktop Duplication rejects the detected
window source. It cannot complete that automatic game-only sequence. Preserve the
error as a failed acceptance result. Do not enable WGC or substitute display capture
implicitly. An explicitly configured display recording with game detection off
is a separate encoding/audio control and includes overlapping windows.

## Visual and audio reference

- Red and blue blocks at the upper left check channel order and SDR colors.
- A green square moves horizontally; 16 binary cells show the rendered frame
  counter, least significant bit first. White means one, blue means zero.
- The pulse block is yellow during tone and blue during silence. Audio alternates
  one second of 440 Hz left / 880 Hz right and one second of silence, at 48 kHz.
  The visual uses the output device's playback position. Queue/device/display
  latency remains: this is not a precision A/V synchronization instrument.
- The window title identifies the adapter, current mode, frame and audio position.
  Audio runs on its own worker; interactive dragging/resizing pauses rendering
  inside the Win32 modal loop. Test recovery after resize, not motion continuity
  during that fixture-imposed pause.
- F11 toggles borderless; F10 requests DXGI exclusive fullscreen; Esc closes.
  `--borderless` / `--exclusive` request a startup mode. Exclusive state is queried
  from DXGI and failed transitions surface an error. This does not prove which
  physical scanout path Windows fullscreen optimizations chose.
- Default lifetime is 180 seconds; `--seconds` accepts 1 through 600.

Probe each presentation/mode separately with a fresh output directory:

```powershell
.\dwm_probe.exe --list
.\dwm_probe.exe --hwnd 123456 --seconds 30 --fps 60 --out mock-flip-windowed
```

Compare first/middle/last BMPs to the visible scene, then test overlap, resize,
minimize/restore and sustained operation. Check actual absence of a yellow border
on the live desktop. Neither changing hashes nor DWM update IDs establish unique
game frames, tear-free output or synchronized audio. Mock results do not establish
real game, anti-cheat, HDR or device-loss compatibility.

Results: [Windows 10 mock validation](research/2026-09-08-mock-game-validation.md).
