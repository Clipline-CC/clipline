# D3D9 producer: traced exclusive capture still freezes

The independent D3D9Ex rendering fixture reproduces the fullscreen freshness
failure on Windows 10 Home 19045 / Radeon 780M / AMD driver 32.0.31041.1004.
PrintWindow and both DWM readers work before and after exclusive, but retain
counter 1311 during concurrent Hardware: Legacy Flip presentation. This is a
different producer API from the earlier D3D11 mocks, not just another reader.
No experimental mechanism was integrated into production recording.

## Fixture and validity controls

The original local fixture reuses the first-party mock's colors, binary counter,
moving square and stereo tones, replacing the renderer with hardware D3D9Ex.
It selects the adapter by window monitor and logs AMD vendor/device IDs and LUID.
CreateDeviceEx/ResetEx use D3DSWAPEFFECT_DISCARD and interval ONE. Every frame is
fully redrawn. There is no frame export, capture API or cooperation with readers.

ResetEx uses fresh presentation parameters and a fullscreen display-mode struct
only for exclusive. After each reset the fixture explicitly sets the full
viewport and checks actual swap-chain Windowed state, back-buffer dimensions,
format, swap effect and adapter display mode. D3D calls stay on the creating
thread; window messages only request transitions. These controls follow the
Microsoft contracts for [CreateDeviceEx](https://learn.microsoft.com/en-us/windows/win32/api/d3d9/nf-d3d9-idirect3d9ex-createdeviceex)
and [ResetEx](https://learn.microsoft.com/en-us/windows/win32/api/d3d9/nf-d3d9-idirect3ddevice9ex-resetex).

The final fixture logs each non-S_OK PresentEx result. Secure-desktop occlusion
is tolerated, and S_PRESENT_MODE_CHANGED requests reset and revalidation before
continuing. These are documented [PresentEx statuses](https://learn.microsoft.com/en-us/windows/win32/api/d3d9/nf-d3d9-idirect3ddevice9ex-presentex),
not proof that a new frame reached the display. Other errors abort the fixture.

The copied executable's fullscreen-optimization compatibility setting was
temporarily disabled. The user accepted elevation for the verified Intel-signed
PresentMon 2.5.1 tracer only; the fixture, capture probes and Clipline were launched
normally. The trace is PID-scoped, QPC-timestamped and bounded to 60 seconds.
PrintWindow runs four seconds at 15 Hz; each DWM reader runs three seconds.
Every capture worker has an external deadline. Borrowed shared handles remain
borrowed, with no writes or CloseHandle calls on those handles.

## Results

The initial windowed sanity control passes: PrintWindow has 50 valid reads and
49 counter advances (107 to 340); the Rust DWM reader has 89 reads/88 changes;
D3D9 readback has 28 valid reads and 27 unique counters under both zero-input
and retained-LUID policies. That run was untraced and is not an overlap test.

The final traced run contains 3,585 target events with D3D9 runtime metadata.
All 448 geometry/foreground samples retain focus and 1280x720 client and monitor
dimensions. The producer logs `windowed=0 size=1280x720` before exclusive sampling,
then `windowed=1 size=1280x720` after restoration. No non-S_OK PresentEx event
overlaps any capture stage. Every exclusive stage has concurrent Legacy Flip
events; before/after stages use Composed: Copy with GPU GDI.

| Reader | Before borderless | Native exclusive | After borderless |
| --- | --- | --- | --- |
| PrintWindow flags=2 | 51 valid reads, 500-735 | 51 valid reads, all 1311; 267 Legacy Flip events | 51 valid reads, 2392-2628 |
| Rust DWM/D3D11 | 88 reads, 88 hashes | 89 reads, one hash; 184 Legacy Flip events | 89 reads, 89 hashes |
| DWM/D3D9, zero LUID/flags | 28 valid reads, 941-1113 | 28 valid reads, all 1311; 189 Legacy Flip events | 28 valid reads, 2827-3000 |
| DWM/D3D9, retained LUID/zero flags | 28 valid reads, 1131-1302 | 28 valid reads, all 1311; 190 Legacy Flip events | 28 valid reads, 3017-3190 |

During exclusive sampling the live title advances 1425 to 1680 for PrintWindow,
1695 to 1860 for Rust DWM, 1875 to 2055 for D3D9 zero-input and 2070 to 2250 for
D3D9 retained-LUID. All capture APIs return without errors, but all four exclusive
probes exit 1 for insufficient motion. D3D9 probe exit alone is not acceptance:
its decoded counter CSV and saved images must also pass freshness checks.

Offline decoding of Rust DWM BMPs gives before counters 750/836/925, exclusive
1311/1311/1311, and after 2637/2726/2815. Inspected images show the correct colored
fixture; the exclusive content is a stale game frame rather than the white/black
surface observed with the D3D11 flip producer. All exclusive PrintWindow and D3D9
snapshots also decode to 1311. DWM/D3D9 BMPs are byte-identical within this exclusive
phase; PrintWindow BMPs have their own identical file hash.

This confirms the tested failure extends to this D3D9Ex producer under measured
native exclusive. It does not prove every D3D9 mode or every possible mechanism
impossible. No end-to-end recording or soak was warranted for a source that
already fails freshness. Overlap exclusion, actual yellow-border absence,
audio/video synchronization, sustained performance and real-game compatibility
are not newly established by this control. Changing hashes/counters would not
prove synchronized, tear-free frames either.

## Evidence, setup defects and cleanup

Local root: `C:\Users\Dain\Desktop\CliplineD3D9Producer-20260908-194923`.
The complete traced run is `matrix-20260908-222954`; the earlier windowed positive
is `windowed-20260908-195318`. The final mock adds mode-change recovery after that
windowed run; its borderless positives in the traced run validate the rebuilt
binary. Source/build scripts, worker copy, trace, stage QPC records, geometry,
producer status logs, snapshots, counter decoding, classification and acceptance
summaries stay in the local evidence root.

| Artifact | SHA-256 |
| --- | --- |
| Final D3D9 producer | `FB349DB8D51C9903D7705FE5F72E0A67CF26E98171CC0E6C189D30114FFBCC1F` |
| PresentMon CSV | `1A312156D9118ADD6AA025E4E1F6F942D3CF0ED93D84457DB601E07E7D98E1CF` |
| Each exclusive DWM/D3D9 BMP | `3C0AABE52EB425DB1BE3DB134E44EC5661F0546E44D371F4F123C15C433325E4` |
| Each exclusive PrintWindow BMP | `CC14123C66191409C32A0D7E68EB3AABE36FBEC0FA49C4987C5C6EB1BDF98F33` |

Two local harness defects were fixed before interpreting the final run:
PrintWindow's mock-name guard needed the new fixture name in a local worker copy;
and the new producer initially aborted on S_PRESENT_MODE_CHANGED after UAC.
The first accepted tracing attempt consequently exited before capture. Canceled
UAC attempts and these setup failures are excluded from capture evidence. No
system policy or permanent elevation change was used to get past consent.

The copied compatibility value was restored to absent and independently checked;
the fixture, reader and trace processes exited. Clipline remains open and paused.
This repository follow-up contains the plan and evidence documentation only.
The production implementation and its prior local quality gates are unchanged;
Windows/Ubuntu CI is checked again at the pushed documentation head.
