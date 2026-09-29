# DWM and thumbnail capture under measured native-exclusive presentation

## Result

The owned-window thumbnail plus PrintWindow combination captures a windowed flip
mock, but did not provide current frames during measured native-exclusive
presentation. Direct DWM shared-surface capture also remained static. Both
exclusive stages ran under `Hardware: Legacy Flip`; showing the thumbnail host
before entering fullscreen did not silently turn these samples into a composed
presentation control.

This closes these bounded DWM/thumbnail experiments without a production backend
change. It does not prove that all possible game-only APIs are impossible. Earlier
unclassified tests are not retroactively assigned a presentation mode.

Machine: Windows 10 Home 19045, Radeon 780M, AMD 32.0.31041.1004. Original mock
hash `98A0E76A3B8D2918B8C2C14790B2552B7D2F3959A1A950E6376BEE3F9486FF7F`.
Evidence root: `C:\Users\Dain\Desktop\CliplineWindowFollowup-20260908-153615`.
Raw screenshots remain local, including surrounding desktop context.

## Windowed thumbnail control

Earlier research had tested a live DWM thumbnail but read its host through the
DWM shared-surface probe, which returned only the host background. This test
instead reads that owned host using PrintWindow flags=2.

An ordinary windowed flip mock supplied an 800x450 client thumbnail in an owned
magenta-background WinForms window. Eight seconds produced 101 API-successful
reads and no API errors, with mean call time 10.65 ms. First/middle/last BMPs
contain correct red/blue reference anchors and valid counters 214, 453 and 688.
Live first/last desktop views also show the game thumbnail. This establishes
sampled motion in those snapshots; the 101 reads were not all counter-validated,
and no encoder, audio, replay, throughput guarantee or sustained stability test
was added for this route. Evidence: `thumbnail-print` and
`thumbnail-snapshot-counters.json`.

Microsoft documents thumbnails as a relationship between a source and an owned
destination top-level window; rendering goes to that destination. It does not
promise a readable capture texture or PrintWindow inclusion. Sources:
[registration](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/nf-dwmapi-dwmregisterthumbnail),
[thumbnail overview](https://learn.microsoft.com/en-us/windows/win32/dwm/thumbnail-ovw).

## Native-exclusive controls

The final run is `exclusive-20260908-154328`. The same mock binary was copied as
`CliplineMockFlipCaptureControl.exe`, and only its per-user Disable fullscreen
optimizations value was set temporarily. The thumbnail host was registered and
shown first, without topmost status for this test. The game was then focused and
requested fullscreen; the host stayed behind it. The host updated its destination
rectangle from the source's client dimensions, now 720x480.

The verified Intel-signed PresentMon 2.5.1 child ran elevated with user consent,
PID-scoped to source 1720 for 20 seconds. Game, thumbnail host and capture probes
remained unelevated. QPC timestamps correlate the stages directly at frequency
10,000,000. Host PID was 15228. All 213 fixture telemetry rows retained game
foreground, and live titles advanced from frame 900 to 2310.

| Stage | Duration/result | Concurrent presentation |
| --- | --- | --- |
| Direct `dwm_probe --require-motion` | 5.034 s, 148 readable samples, zero changes/errors, exit 1 | 306 events, all Legacy Flip |
| Thumbnail host, PrintWindow flags=2 | 6.085 s, 77 API-successful reads, zero API errors, invalid initial coordinate-based counter checks, exit 1 | 388 events, all Legacy Flip |
| Entire trace | 1,202 events, one source swapchain `0x15EF0AD6150` | All Legacy Flip |

Direct DWM BMPs show unchanged white/black content at 720x480. The export's update
ID was 1586710 in the initial sample; its pixel hash did not change during the
probe. First/middle/last BMPs are byte-identical. A successful shared-surface read
therefore did not establish useful game pixels.

The thumbnail BMPs contain a game image, but it is stale. The live source changed
to 720x480 while thumbnail content retained the older 800x450 layout, stretched
into the new destination. The original counter-coordinate check was therefore not
sufficient to classify freshness. Offline decoding with x scale 720/800 and y
scale 480/450, plus host-client offset (8,31), finds valid counter **304** in all
three saved images. All three BMPs are byte-identical while the live source has
already advanced beyond 900. This establishes stale snapshots independently of
the validator's coordinate failure; it does not claim hashes were collected for
every thumbnail read.
The worker's 77 invalid-counter verdicts are not used as freshness evidence.

| Artifact | SHA-256 |
| --- | --- |
| PresentMon CSV | `02519B99221ED96C8C211313C0908A935849D1433388C7949525F2AFB24C3143` |
| Each direct DWM BMP | `43E5FFFED435E39CDAAD04B5062E3A8D496476754B9DAC9A83BABA01BC000169` |
| Each thumbnail PrintWindow BMP | `FC33D64D92733B5AD342F62F06BCD54D27F93C5C1D4B9364454ACDA37E219BFB` |

## Bounds and cleanup

The source and host identities and capture affinity were checked before and
after each PrintWindow read; the source lifetime was retained and checked too.
The host checked source affinity while its thumbnail relationship was active.
Workers were externally supervised with finite deadlines. DWM's borrowed shared
handles were not closed, and no WGC, injection, protected-window bypass or display
capture fallback was used. Live desktop images were observation evidence only.

The earlier `exclusive-20260908-154031` attempt ended in Windows elevation
cancellation before capture. Its copied-fixture setting was restored, and the
attempt is excluded. The successful rerun also restored the compatibility value
to absent, verified independently afterward. All test processes exited; no
global/account settings, drivers or runtimes were changed.

These experiments used the existing repository probes plus local bounded
thumbnail harnesses; there is no production code change or new capture backend.
The next investigation would need a different mechanism, rather than treating
successful API calls, thumbnail visibility or stale game pixels as acceptance.
