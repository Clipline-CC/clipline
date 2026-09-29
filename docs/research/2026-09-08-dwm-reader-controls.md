# DWM alternate reader and buffer preservation controls

On Windows 10 Home 19045 / Radeon 780M / AMD driver 32.0.31041.1004,
an original D3D9Ex reader successfully reads moving blt-window content from the
DWM shared allocation. It does not recover flip-window content. Changing the
mock from FLIP_DISCARD to FLIP_SEQUENTIAL also does not recover fresh fullscreen
pixels. No production code or capture source selection changed.

**The two new fullscreen matrices are untraced.** Windows canceled the PresentMon
UAC launch before capture in the initial setup attempt. Subsequent runs explicitly
skipped tracing and recorded `PresentationTraced=false`. Their "exclusive" stages
mean fixture-reported fullscreen, not measured native-exclusive ownership. Earlier
[traced constant-resolution and elevation failures](2026-09-08-fullscreen-surface-continuity.md)
remain separate evidence and cannot classify these new runs retroactively.

## ABI and reader controls

Read-only inspection of this host's Microsoft-signed user32.dll (file version
10.0.19041.1, export RVA 0x2E7C0) supports the existing six-argument calling shape.
Its wrapper reads argument 3 (adapter LUID) and argument 5 (flags) before calling
NtUserGetWindowCompositionInfo, then writes them afterward. Treating these as
pure outputs is therefore incomplete on this build. This is an observed private
implementation, not a supported Windows contract.

The current Rust probe initializes these values to zero each time. A local
diagnostic compared zero initialization with retaining the returned LUID or flags.
The returned raw format was 87, matching the verified DXGI BGRA texture layout,
not the historical D3DFORMAT interpretation. Returned flags were 8; their meaning
was not inferred from an unrelated flags enum and unknown bits were not tried.

The original local reader opens the borrowed handle on the producer's adapter
with D3D9Ex CreateTexture, copies through GetRenderTargetData to owned SYSTEMMEM
staging, and reads pixels through LockRect. D3D11 opens that same allocation only
to validate dimensions/layout; it performs no pixel copy. Window identity,
lifetime, visibility and capture affinity are checked. Shared handles are never
closed or written to. Synchronous work has an external process deadline.

Microsoft documents the matching-size/format and DEFAULT-to-SYSTEMMEM restrictions
for [GetRenderTargetData](https://learn.microsoft.com/en-us/windows/win32/api/d3d9/nf-d3d9-idirect3ddevice9-getrendertargetdata),
and the supported subset of [D3D9/D3D11 shared-resource interop](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11device-opensharedresource).
These contracts support the reader control, not the undocumented DWM export.

Five-second blt positive controls using the final diagnostic binary:

| Metadata input | Reads / pixel changes / errors | Decoded source counter |
| --- | --- | --- |
| Zero LUID and flags each call | 46 / 45 / 0 | 71 to 361 |
| Retain returned LUID, reset flags | 46 / 45 / 0 | 379 to 671 |
| Retain returned flags, reset LUID | 1 / 0 / 45 | Initial 689; then surface unavailable |

Saved positive BMPs show the colored fixture and counter. Retaining the adapter
ID works here; feeding returned flags back does not. There is no reproduced need
to replace the existing probe's zero initialization.

## Flip preservation matrix

A separate fixture changes exactly one source line: FLIP_DISCARD becomes
FLIP_SEQUENTIAL. It retains the same animation, audio and fullscreen implementation
and adds no cooperating capture code. Microsoft specifies buffer preservation for
[FLIP_SEQUENTIAL](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/ne-dxgi-dxgi_swap_effect),
which makes this a control for discarded-buffer behavior, not a general game fix.

Both original and sequential fixtures run borderless -> reported exclusive ->
borderless at constant 1280x720, with the copied executable's fullscreen
optimizations disabled temporarily. All 367 original and 369 sequential telemetry
rows retain foreground and matching client/monitor geometry. Each phase runs
PrintWindow flags=2 for four seconds, then the Rust DWM reader, D3D9 zero-input
reader and D3D9 retained-LUID reader for three seconds each.

| Fixture / reader | Before borderless | Reported exclusive (untraced) | After borderless |
| --- | --- | --- | --- |
| Original / PrintWindow | 51 valid reads, counters 461-695 | 51 valid reads, all 1273 | 50 valid reads, counters 2350-2582 |
| Sequential / PrintWindow | 51 valid reads, counters 466-700 | 51 valid reads, all 1277 | 51 valid reads, counters 2351-2587 |
| Either / Rust DWM | 88 reads, zero changes | 88 reads, zero changes | 88 reads, zero changes |
| Either / D3D9, either metadata policy | 28 reads, zero changes | 28 reads, zero changes | 28 reads, zero changes |

During exclusive PrintWindow sampling the original live title advances 1395 to
1635 and sequential advances 1395 to 1650, while captured counters stay frozen.
All DWM/D3D9 snapshots contain the same white/black non-game content and share
the same BMP SHA-256. A successful API read is not a successful frame capture.
Every DWM/D3D9 stage and exclusive PrintWindow stage exits 1; both borderless
PrintWindow controls pass. D3D9 reports zero API errors in these flip stages.

These results do not establish native-exclusive presentation in this batch,
universal API impossibility, real-game compatibility, yellow-border absence,
tear-free frames, or sustained performance. No long recording was warranted for
a source that already failed pixel freshness. The alternate reader and buffer
preservation controls supply no passing fullscreen mechanism to integrate.

## Evidence and cleanup

Local root: `C:\Users\Dain\Desktop\CliplineDwmReaderControls-20260908-183424`.

- `positive-20260908-184131`: final reader positive controls.
- `discard-20260908-184616` and `sequential-20260908-184843`: complete matrices,
  per-stage CSV/BMP/summary/exit records, foreground/geometry telemetry,
  `NOT-TRACED.txt`, classification summaries and snapshot hashes.
- `dwm9_probe.cpp`, build script/binary, sequential fixture source/build/binary,
  `run-reader-matrix.ps1` and `analyze-matrix.ps1`: original local diagnostics.
- `discard-20260908-184247`: canceled tracing setup; no capture result.

| Artifact | SHA-256 |
| --- | --- |
| Final D3D9 diagnostic executable | `0D4EF64285F103BB3EDB4B5832B91511B90E1E262D80254CE9F58539E26C21CA` |
| Original flip fixture | `98A0E76A3B8D2918B8C2C14790B2552B7D2F3959A1A950E6376BEE3F9486FF7F` |
| Sequential fixture | `CBA3B03603640D15BFF55999552F92ADADECCAE93B38692A002B6F37EFB96442` |
| Every flip DWM/D3D9 BMP in these matrices | `598EB6A5B7E1D3291E1E195CA43891F66990FCFB3307CC205D208D4C9CAA8CAB` |

Initial local-reader runs rejected raw format 87 before the verified DXGI-to-D3D9
mapping was added; those harness failures are excluded. A later PowerShell 5.1
analysis serialization error exhausted memory after capture completed. Parsing
summary files into plain key/value objects fixed that local analysis defect;
both datasets then analyzed successfully under 12-second/512-MiB limits. It was
not a capture memory result, and raw evidence was unchanged.

Copied-fixture compatibility values were restored to absent and independently
checked. Mocks, readers and trace helpers exited; Clipline remains open and paused.
No WGC, monitor fallback, injection, protected-window bypass, global setting or
driver change was introduced. System DLLs/disassembly and raw desktop evidence
stay local. Production tests/Clippy from the preceding code revision remain
applicable; this follow-up changes documentation only, with CI checked after push.
