# Performance first pass — MP4 batching and process-query reuse

Implementation starts from `develop` at `1ea5f854`. The user selected these two
opportunities from the supplied audit of `e8076200`.

## Changes

`HybridMp4Writer::write_planned_fragment` batches each nonempty fragment through a
temporary standard-library `BufWriter` with a 256 KiB capacity. Replay, full-session
recording, and file/memory trim transports share this helper. The complete fragment
is explicitly flushed before sample tables, decode times and sequence numbers advance.
Public writer APIs, bytes, timestamp gaps, seek offsets and finalization stay compatible.
This preserves completed-fragment visibility, without adding a power-loss durability
guarantee. Failed output remains an unfinished file that callers must abandon.

Visible-window enumeration owns a PID-to-path map, including unsuccessful queries,
and one lazily allocated 32,768-element UTF-16 scratch buffer. Each new enumeration
starts empty; titles, visibility and ordering are still read per window. Query handles
close immediately. This is scan-local discovery metadata; persistent PID caching or
security decisions would need live process-instance validation.

## Real-file check

Windows 11 Pro build 26200, Ryzen 7 5700X3D (8 cores/16 logical processors),
rustc 1.98.0. MP4 dependencies were built with `--release`; the standalone harness
used `rustc -O`. Each synthetic file represents 30 seconds at 60 FPS with 0.5-second
fragments, 25,000-byte video samples, and 480-byte/20-ms audio packets. Seven rounds
rotated the baseline buffer-size order (0/16/64/256/1024 KiB). The production change
was measured separately in seven rounds with plain real-file sinks.

Every real file matched its memory reference. Patched references also matched the
baseline finalized files byte for byte for all three track configurations. Times below
are medians for muxing/writing; `File::sync_data` was timed separately, outside that
column. Underlying writes count calls to the real file's `Write` implementation.

| Audio tracks | Previous writes | Patched writes | Previous mux ms | Patched mux ms | Previous sync ms | Patched sync ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 3,425 | 185 | 45.459 | 24.119 | 18.765 | 18.344 |
| 2 | 4,925 | 245 | 46.996 | 20.630 | 17.773 | 19.591 |
| 17 | 27,425 | 245 | 139.989 | 29.306 | 21.396 | 23.076 |

The baseline 256 KiB wrapper captured most of the write-call reduction. A 1 MiB
wrapper saved another 2–4 ms per file while quadrupling buffer capacity. The shared
production helper uses 256 KiB, adds one bounded allocation per nonempty fragment,
and does not retain buffered data between calls. Source-backed transforms also retain
their existing 64 KiB copy scratch.

These are local synthetic save/write measurements. Filesystem caching, background
load and power state were not controlled. They establish neither gameplay FPS nor
GPU/whole-app CPU gains, and do not benchmark decoder playback or slow storage.
Per-scan process-query reuse has a correctness regression, without a CPU timing claim.
Evidence remains in ignored `target/performance-first-pass/`: harness, CSVs, baseline
reference files, and workspace gate logs.

## Permanent checks

The counted-sink regression failed before batching (502 writes for 500 packets);
the explicit flush-error check also failed before the fix. The final fixture uses
2,000 packets, crossing buffer capacity, compares independently constructed fragment
bytes while the writer is live, and checks write, flush and truncated-source errors
without advancing fragment bookkeeping. Existing transport, seek/table, multitrack,
timing-gap, full-session and disk/memory replay tests cover the shared consumers.

Win32 tests query the real current process, prove that cached successes and failures
leave scratch untouched, and prove a new scan queries fresh metadata. Existing live
enumeration and game-selection tests remain the integration checks.
