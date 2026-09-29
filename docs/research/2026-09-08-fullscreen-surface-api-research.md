# Web and local API investigation after the D3D9 control

The expanded search has not identified a new callable mechanism that gives
Clipline fresh, isolated pixels from an unmodified native-exclusive game on
Windows 10 without WGC or injection. This is a research result, not a proof that
every private Windows mechanism is impossible. The
[traced D3D9 experiment](2026-09-08-d3d9-producer-control.md) remains the latest
capture test; this follow-up performs no capture or system-setting changes.

## Legacy DWM export: present, but not an independent read-only capture contract

Microsoft's [DwmDxGetWindowSharedSurface documentation](https://learn.microsoft.com/en-us/windows/win32/dwm/dwmdxgetwindowsharedsurface)
describes an HWND/adapter entry point for graphics drivers and runtimes on Windows
7. It returns a surface to update, with corresponding update notifications even
when no pixels were changed. The documentation explicitly excludes application
callers. This differs from the six-argument user32 export used by our probe.

Read-only export inspection of the local, Microsoft-signed dwmapi.dll confirms
ordinal 100 is present as `DwmpDxGetWindowSharedSurface`, RVA 0x5F70. The earlier
support-contract objection must not be restated as "the function is absent."
Static inspection finds a concrete connection to the tested mechanism: one
acquisition branch calls user32's `DwmGetDxSharedSurface` at RVA 0x6121 through
IAT RVA 0x19560. The user32 import table starts at RVA 0x194B8; this is entry 21
(zero-based, eight-byte entries). This establishes one shared path, not that
every branch is equivalent or that the private function has been capture-tested.

We did not invoke the driver/runtime API, submit producer updates, or try unknown
flags. A different function name or returned handle would not by itself establish
freshness, producer ownership, or a correct read-only synchronization protocol.

## Presentation history does not supply the missing resource-opening contract

[D3DKMTGetPresentHistory](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmthk/nf-d3dkmthk-d3dkmtgetpresenthistory)
retrieves history tokens for an adapter. Its
[request structure](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmthk/ns-d3dkmthk-_d3dkmt_getpresenthistory)
contains an adapter and token buffer, not an HWND/PID frame selector.
[Present-history tokens](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmthk/ns-d3dkmthk-_d3dkmt_presenthistorytoken)
are submitted by the rendering application to tell DWM a buffer is ready.

The [flip token](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmthk/ns-d3dkmthk-_d3dkmt_flipmodel_presenthistorytoken)
contains a logical-surface identifier and composition metadata. That is not a
documented promise that the identifier can be passed to OpenSharedResource.
[D3DKMT_OPENRESOURCE](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmthk/ns-d3dkmthk-_d3dkmt_openresource)
already requires a shared-resource handle as input. The reviewed contracts do
not provide a conversion from another game's history token to such a handle.

This distinction also applies to ETW/PresentMon: knowing which process presented
a frame is not access to its pixel allocation. The conclusion is the missing
documented bridge, not a claim that internal Windows consumers cannot access
surfaces. No compositor history queue was consumed or intercepted for this audit.

Microsoft's [GetSharedHandle contract](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiresource-getsharedhandle)
requires an existing shared resource, fails for non-shared resources, and prohibits
CloseHandle/DuplicateHandle on legacy DXGI shared handles. Exporting a texture
from our own mock would demonstrate cooperation, not acquisition from an
unmodified game. No producer-handle harvesting or protection bypass was attempted.

## DirectComposition wrappers and newer presentation APIs

[CreateSurfaceFromHwnd](https://learn.microsoft.com/en-us/windows/win32/api/dcomp/nf-dcomp-idcompositiondevice-createsurfacefromhwnd)
wraps a **layered** window's rasterization for another composition visual. The
documented output disappears when the source ceases to be layered. It is not a
general arbitrary-swap-chain acquisition contract. Restyling the game to satisfy
this condition would alter the producer being tested and require separate
presentation/performance validation.

[CreateSurfaceFromHandle](https://learn.microsoft.com/en-us/windows/win32/api/dcomp/nf-dcomp-idcompositiondevice-createsurfacefromhandle)
and [DCompositionCreateSurfaceHandle](https://learn.microsoft.com/en-us/windows/win32/api/dcomp/nf-dcomp-dcompositioncreatesurfacehandle)
wrap/create shared composition objects. They do not eliminate the need to obtain
the relevant producer's resource through an appropriate sharing contract.
The newer [composition swapchain programming model](https://learn.microsoft.com/en-us/windows/win32/comp_swapchain/comp-swapchain)
requires Windows 11 build 22000.194 or later and is a presentation API, not an
arbitrary-game capture interface that a newer SDK adds to build 19045.

Local export tables also contain `NtVisualCaptureBits`, `NtDesktopCaptureBits`
and `NtDCompositionGetFrameSurfaceUpdates`; dcomp ordinal 1028 forwards to
NtDesktopCaptureBits. Their presence is recorded as an unresolved private-API
lead only. The search did not establish a callable ABI, authorized per-window
pixel-access contract and freshness/synchronization semantics for native-exclusive
capture. We did not infer these from the names or invoke unidentified ordinals.

## Composition workarounds are a different experiment

Microsoft explains that [Fullscreen Optimizations](https://devblogs.microsoft.com/directx/demystifying-full-screen-optimizations/)
can run a game that believes it is exclusive through optimized borderless
presentation. Showing an overlay can return that path to composition. A solution
that depends on forcing composition would therefore need its own presentation,
latency, overlay-exclusion and compatibility evidence; it would not establish
capture while the game retains native-exclusive ownership.

The [AMD AMF display-capture contract](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/blob/master/amf/doc/AMF_Display_Capture_API.md)
selects a monitor, including when its timing follows an application's fullscreen
flips. Better presentation timing does not establish a window-only source.
Neither a composition workaround nor a display source was silently selected.

## Outcome and next evidence needed

No source found in this review justifies another capture run as a promising fix.
The unresolved question is how a separate process can obtain a fresh, readable
game-owned fullscreen allocation under the existing constraints. A useful new
lead must specify acquisition, access/protection, resource lifetime and frame
synchronization, then survive the existing traced before/exclusive/after control.
Further reader or mock permutations alone would not answer that question.

An independently reviewed local evidence package is prepared for a focused
Microsoft/AMD technical inquiry; it has not been sent or uploaded. The inquiry
asks whether such an access contract exists and whether the observed stale DWM
surface is expected in Legacy Flip. No external contact is implied or authorized
by preparing the package.

Local audit root:
`C:\Users\Dain\Desktop\CliplineSurfaceApiResearch-20260908-225041`.
It contains export/import listings, local static-inspection evidence and module
identities/hashes. All inspected DLL signatures are Valid. Versions:
dwmapi/user32 10.0.19041.1, dcomp/gdi32 10.0.19041.6157, win32u 10.0.19041.6456.
SDK declarations were read from installed SDK 10.0.26100.0; those declarations
are not evidence that every declared feature exists on this OS. System DLLs and
disassembly remain local. The evidence package excludes them and desktop views.
This repository change is documentation only; production capture remains separate.
