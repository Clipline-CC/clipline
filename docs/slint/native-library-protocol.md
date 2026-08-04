# Native Slint Library Evidence Protocol

This protocol defines the Milestone 7 Library/Cloud measurement boundary. It supplements
`baseline-protocol.md`; it does not replace or relax that protocol's process identity, memory,
provenance, or environmental-noise rules.

## Evidence status

The harness and sampler implementation may be committed before accepted measurements exist.
Implementation tests, dry runs, or rejected samples are diagnostic evidence only. An absolute gate
is passed only by at least three accepted samples for the same scenario and clip count. Matched
Tauri, real-account, Narrator/UI Automation, high-DPI, Win10/Win11, and real-GPU gates remain
explicitly pending whenever the required environment is unavailable.

Automated runs must never read or mutate the installed Clipline profile, saved credentials, or a
production Cloud account.

## Frozen evidence — `3e07a95`

Milestone 7 is implementation-complete at `3e07a95`, but its absolute acceptance gate is **NO-GO
pending quiet-host evidence**. No performance or lifecycle threshold failed. Every completed sample
below was rejected by the protocol's environmental-noise rule before gate evaluation, so the values
are diagnostic evidence only and do not pass an absolute gate.

The frozen benchmark executable is
`apps/clipline-slint-spike/target/benchmark/examples/catalog_harness.exe`, SHA-256
`729f74cfd18555a580aa9fe07d82ff6372743142aba67adbe1e00f1bac9c7c4a`, 23,327,744 bytes. The
source oracle is `fixtures/playback/h264-one-opus-3s.mp4`, SHA-256
`cc925d7d111fde927d9a2e3666731b6b4403f065c83b1b475bace58ea73b7bb3`. Runs used Windows 11
build 26200, 100% display scaling, the Microsoft Basic Display Adapter, and `winit-software`.

| Scenario | Accepted / rejected | Diagnostic result | Evidence |
|---|---:|---|---|
| 2,000-clip `local-cold`, 300-second steady window | 0 / 6 | Five complete attempts were rejected solely for background CPU noise (6.45%–15.08% noisy samples versus the 5% limit). They reported first usable page 601.3261–1091.4355 ms, PWS p50 29,749,248–31,813,632 bytes, PWS p95 29,913,088–32,043,008 bytes, CPU p95 0.4787%–0.5458%, and clean bounds/lifetimes. The sixth attempt was externally interrupted and is invalid. | `artifacts/slint-library-m7-final-2000-cold/20260804T005541Z-slint-library-2000-local-cold-series-4ce65e89.json` plus the five complete raw/provenance pairs in that directory |
| 50-clip `reveal-close-100`, 300-second steady window | 0 / 1 | Rejected for 14.516% background-noise samples. Diagnostic counters show 100 window cycles, 100 Cloud open/replace/close cycles, two cache fills, 200/200 balanced leases, zero active leases, 372,736-byte PWS growth, 27,369,472-byte PWS p50, and 0.4895% CPU p95. | `artifacts/slint-library-m7-final-lifecycle-diagnostic/20260804T010201Z-slint-library-50-reveal-close-100-series-bec6133b.json` |

The unrelated installed Clipline process PID 5548 was excluded, recorded, and never killed. The
automated Cloud lifecycle used a disposable local cache and hash-verified fixture; it did not read
credentials, access the network, mutate a production account, or touch the installed profile.

The required three accepted samples remain missing. Accepted 50/500/2,000 cold and warm matrices,
synthetic Cloud, churn, and lifecycle scenarios remain open, as do matched Tauri, real-account,
Narrator/UI Automation, DPI/OS, and real-GPU gates. Implementation work may continue because the
stop condition is an evaluated absolute-gate failure, and no rejected sample was evaluated as a
pass or failure. Cutover and verified parity remain prohibited until the missing evidence passes.

## Fixture ownership and provenance

The sampler creates a unique disposable profile, fixture root, and seed root before starting the
measured process or first-usable-page timer. NTFS cannot carry 2,000 links to one inode, so the seed
root contains `ceil(clipCount / 500)` ordinary copies of one manifest-covered source oracle. Each
seed is SHA-256 verified once and backs at most 500 regular MP4 hard links. The harness checks every
visible clip's file identity against that bounded seed set. Fixture construction therefore does
not inflate candidate startup latency or measured memory, and copied/foreign clips fail closed
without hashing all 2,000 visible links during startup.

Every run records:

- source fixture canonical path and SHA-256;
- seed-root path, seed count, 500-link cap, and seed SHA/file-identity verification;
- requested and observed clip count;
- fixture root and disposable profile root;
- commit SHA and executable SHA-256;
- frontend, renderer, GPU/adapter, display scale, OS, and logical CPU count;
- root PID, process name, and creation time;
- all unrelated Clipline PIDs observed during the run.

The harness revalidates the completed fixture root before publishing `ready`. A missing, foreign,
non-regular, hash-mismatched, or extra clip fails closed. The sampler owns and cleans only the
processes and files created for that run. Unrelated Clipline processes are excluded from the owned
process tree and recorded in provenance; they are never killed and do not by themselves abort a
Slint absolute run.

## Harness command contract

`catalog_harness` accepts these stable arguments:

```text
--fixture-root <path>
--fixture-seed-root <path>
--source-fixture <path>
--source-sha256 <lowercase hex>
--clip-count <50|500|2000>
--scenario <name>
--marker-path <create-new path>
--stop-path <absent-at-start path>
--exercise-path <absent-at-start path>
--telemetry-path <create-new path>
--renderer winit-software
--build-sha <commit>
--adapter <name>
--scale <positive factor>
```

Supported scenarios are:

- `local-cold`: first usable local page with an empty owned poster cache;
- `local-warm`: the same local page with owned poster results already available;
- `cloud-pages`: deterministic synthetic Cloud pages with no HTTP or credentials;
- `selection-page-churn`: 100 Local/Cloud page switches, acknowledged poster cancellations, and
  upload-progress bursts, one bounded operation per Slint event-loop tick after the sampler's
  steady-window exercise signal;
- `reveal-close-100`: 100 real Slint component create/show/detach/drop cycles through the shipping
  `ShellLifecycle` and desktop attachment path plus 100 deterministic Cloud-media
  open/replace/close cycles through `CloudCache` playback protection. Each Cloud cycle acquires an
  initial lease, overlaps it with the replacement lease, releases the replaced lease, then closes
  the replacement. The fixture transport reads only the already hash-verified local MP4; it loads
  no credentials and performs no network requests. Window, media-open, media-replace, and
  media-close phases advance on Slint event-loop ticks and may not use a pending escape hatch.

The measured process constructs and updates the real Slint Library component and its bounded
models. A headless controller-only simulation cannot satisfy the memory or lifecycle gates.
Synthetic Cloud data must traverse the same bounded projection contracts without network access.

## Marker protocol

Markers are append-only JSON Lines with `schemaVersion: 1`, an RFC 3339 UTC timestamp, and one of
these exact kinds:

- `ready`
- `pageSettled`
- `postersSettled`
- `exerciseSettled`
- `error`
- `stop`

The sampler treats any complete `error` marker as terminal even if a later `ready` marker exists.
Malformed or partially written trailing lines fail closed once the root process exits.
`pageSettled` and `ready` are emitted after the initial bounded projection has survived a real
Slint event-loop/render opportunity, before poster extraction or synthetic scenario work.
`firstUsablePageMs` freezes at that marker. `postersSettled` is emitted only after event-loop-paced
page/filter interactions and the bounded 32-poster window settle. The sampler waits for all three
startup markers before warmup. For the two 100-cycle scenarios it creates `exercise-path` after
the first steady sample and requires exactly one later `exerciseSettled` plus at least one steady
sample after it.

## Final telemetry

Final telemetry is JSON schema version 1 and at most 1 MiB. Its top-level `publication` value is
exactly `create-new-atomic-rename`. The target must not exist before launch. The harness writes a
create-new sibling temporary file, flushes it, and atomically renames without overwriting. The
sampler reads telemetry only after the creation-time-verified root exits and requires the SHA-256
to remain stable across two reads.

Required top-level fields include `status`, `scenario`, `clipCount`, `sourceFixture`, `provenance`,
`metrics`, `lifecycle`, `churn`, `reveal`, and `safety`. `sourceFixture` contains `path` and
lowercase `sha256`; renderer, build, adapter, scale, roots, and process identity live under
`provenance`. `status` is exactly `completed`, and safety must report
`productionCredentialsLoaded: false` and `cloudNetworkRequests: 0`.

Required metrics are:

- `firstUsablePageMs`
- `windowShownModelPublished`
- `pageChangeP95Ms`
- `filterGroupP95Ms`
- `posterSettleMs`
- `retainedRows`
- `retainedDecodedImages`
- `posterLruEntries`
- `ffmpegChildPeak`
- `duplicateSameKeyExtractions`
- `posterExtractionStarts`
- `singleFlightFollowers`
- `offPageDecodedImagesAfterSettle`
- `offPageModelImagesAfterSettle`
- `stalePublications`
- `activeLeasesAfterClose`
- `pwsGrowthBytes`

The harness cannot honestly sample its own process-tree envelope. Its raw final telemetry therefore
sets `pwsGrowthBytes: null` and `pwsGrowthMeasuredExternally: true`. The sampler computes the
numeric value from the owned-tree raw samples and records it in the evidence envelope. A null value
can never pass the growth gate.

`selection-page-churn` additionally records `localCloudPageSwitches`, `posterCancellations`, and
`uploadProgressBursts`, each exactly 100, plus `executedDuringMeasuredWindow: true`.
`reveal-close-100` records `windowRevealCloseCycles: 100`,
`windowRevealCloseCyclesPending: false`, and
`windowCyclesExecutedDuringMeasuredWindow: true`. It also records `cloudMediaCycles: 100`, exact
`cloudMediaOpens`, `cloudMediaReplacements`, and `cloudMediaCloses` values of 100,
`cloudMediaCyclesPending: false`, and `cloudMediaCyclesExecutedDuringMeasuredWindow: true`.
The lifecycle must report exactly 200 acquired and 200 released leases because each cycle contains
one Open lease and one overlapping replacement lease. `cloudMediaCacheFills` is exactly 2: the
initial and replacement assets are filled once from the local fixture, then all later cycles prove
the warm-cache acceptance path.

Image ownership is split but also aggregated. Poster handles cover the bounded Local
`PosterController`; model images cover every Local or synthetic Cloud image clone published into
the real Slint row model. Replacing or clearing a model releases the previous exact count.

Required lifecycle counters are:

- `attachmentsCreated` / `attachmentsDropped`
- `imagesAccepted` / `imagesReleased`
- `posterHandlesAccepted` / `posterHandlesReleased`
- `modelImagesPublished` / `modelImagesReplaced`
- `leasesAcquired` / `leasesReleased`

Every acquired/released pair must balance at shutdown. Counts are monotonic and use checked
arithmetic; overflow fails the run. Telemetry-temp safety is proved by create-new identity-fenced
publication and its foreign-replacement regression rather than hard-coded self-referential temp
counters.

## Process sampling and acceptance

The sampler reuses `Clipline.ProcessMetrics.psm1` and records the same raw process-tree columns as
the frontend baseline. Root identity is PID + process name + creation time. Descendants must be
newer than the verified root and remain in that owned tree. The run records private working set,
private commit, ordinary working set, CPU, handles, threads, process count, GPU counter
availability, and child-read failures.

The warmup, steady interval, sample cadence, and environmental envelope are recorded. A run is
rejected rather than passed when sampling is incomplete, child reads fail beyond the protocol
allowance, root identity changes, readiness or graceful stop fails, telemetry is missing/unstable,
or the host-noise rule from `baseline-protocol.md` rejects the envelope. A rejected run remains
available as diagnostic evidence but does not count toward the required three samples.

Selection churn and reveal/close growth are computed across steady samples that bracket the owned
exercise signal and its completion marker; idle-after-churn sampling cannot pass. Exceptional-path
cleanup refreshes the live descendant set before stopping it child-first. Disposable profiles stay
beside their create-new raw/provenance files as auditable evidence and are removed only by a later
explicit evidence-retention janitor, never by an in-run broad delete.

## Absolute gates

- retained rows are at most 60;
- retained decoded page images are at most 32;
- poster path/status LRU entries are at most 120;
- simultaneous FFmpeg poster children are at most 2;
- duplicate same-key extractions are zero;
- off-page decoded/controller and Slint-model images after settle are zero;
- stale publications and active leases after close are zero;
- every lifecycle ownership pair balances;
- 100-cycle private-working-set growth is at most 10 MiB;
- first usable 2,000-clip page is at most 1.5 seconds on the gate machine;
- five-minute 2,000-clip open-Library private-working-set p50 is at most 140 MiB.

The sampler records p95/max memory and all non-gated counters as diagnostics. It must not rewrite a
failed absolute gate as an environmental skip unless the sample itself was rejected before gate
evaluation.

## Matched gates

Matched Slint and optimized Tauri samples use the same fixture, machine, display/scale, warmup,
steady window, cadence, and noise rules. Slint must have:

- open-Library five-minute private-working-set p50 at most 65% of Tauri;
- private commit at least 25% lower than Tauri;
- page/filter p95 no more than 50 ms slower than Tauri;
- CPU within one percentage point over equal windows.

If the installed Clipline process, missing optimized Tauri binary, or host noise prevents matching,
the matched gate remains pending. No synthetic ratio may substitute for raw paired samples.

## Manual and network-dependent gates

Narrator/UI Automation, keyboard-only traversal, 100%/150%/200% DPI, Win10/Win11, real-GPU, and a
non-mutating real-account smoke are recorded separately. Synthetic Cloud proves bounds and
ownership only; it does not pass real service compatibility. A real-account smoke may list and
open existing data but must not upload, delete, change visibility, or disconnect the account.
Because this harness pins `winit-software`, it always records the real-GPU/media gate as pending;
the presence of a non-Basic adapter alone cannot promote that gate.
