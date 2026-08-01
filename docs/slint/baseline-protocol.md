# Slint matched frontend baseline protocol

This protocol produces the evidence used by the Slint replacement gates. It does not contain a
baseline result: no number is publishable until Tauri and Slint are measured on the same machine,
with the same corpus, settings, window state, renderer declaration, and sampling script.

## Safety boundary

Build the dedicated optimized benchmark profile:

```powershell
cargo build -p clipline-app --profile benchmark
target/benchmark/clipline-app.exe --clipline-benchmark-probe
```

The probe must report `benchmark_shell_safe: true`, non-zero optimization, debug assertions, and
`autostart_registry_mutation: false`. The harness rejects an ordinary debug/release binary and
refuses to start while any `clipline-app` process is running. This matters because a shipping
release startup synchronizes Clipline's global per-user autostart registry entry.

The benchmark profile uses release optimization with debug assertions only to select the existing
registry-safe shell policy. It is not a distributed product build. It still uses Clipline's normal
single-instance identity, so close every installed/development Clipline instance and do not launch
one during a run.

Each run creates a fresh profile below its output directory and sets `APPDATA`, `LOCALAPPDATA`,
`USERPROFILE`, `TEMP`, and `TMP` for the child. Tauri runs also set
`WEBVIEW2_USER_DATA_FOLDER` to a fresh directory and enable the same CDP flag on every Tauri run.
The user's settings/media profile is never copied or rewritten. The generated profile remains with
the raw evidence for inspection; remove it only after the run has been accepted and archived.

## Metric contract

The unit of comparison is the root plus every creation-time-validated descendant. The sampler
records one long-form row per process and repeats tree aggregates on each row.

- **Private working set:** primary RAM gate; physical pages private to each process.
- **Private commit:** committed private virtual memory; recorded separately because trimming a
  working set does not necessarily reduce commit.
- **Ordinary working set:** diagnostic only; summing a process tree double-counts shared pages.
- **CPU:** interval process time divided by wall time and logical processors.
- **Handles, threads, process count:** lifecycle/leak diagnostics.
- **GPU local/non-local allocation:** optional Windows counters. Unavailable is `null` plus
  `GpuCountersAvailable=false`, never zero.

`PROCESS_MEMORY_COUNTERS_EX2.PrivateWorkingSetSize` requires Windows 10 22H2 with the September 2023
cumulative update or a correspondingly updated Windows 11 22H2 or later. The harness preflights a
strict EX2 read before launching Clipline and fails rather than changing metrics. See Microsoft's
[`PROCESS_MEMORY_COUNTERS_EX2` requirements](https://learn.microsoft.com/en-us/windows/win32/api/psapi/ns-psapi-process_memory_counters_ex2).

Task Manager's grouped headline is not interchangeable with these counters.

## Corpus

Generate or validate the first-party decoder corpus before measuring:

```powershell
./scripts/generate-playback-fixtures.ps1 -Mode SelfTest
./scripts/generate-playback-fixtures.ps1 -Mode Validate
```

`fixtures/playback/manifest.json` freezes SHA-256, byte size, exact FFmpeg arguments, tool identity,
stream expectations, and GOP constraints. The four checked-in H.264 High + Opus files are small
decoder/interaction oracles. They are FFmpeg-muxed and explicitly are **not** the production mux
oracle. Before the native media gate, add a separate fixture authored by `HybridMp4Writer` with the
production `shiguredo_opus` path.

Performance-grade 1080p60 media is not committed. Record its hash and provenance in the run output.

## Environment record

Use a quiescent machine and record, alongside the automatic metadata:

- Windows edition/build and pending-restart state;
- CPU, physical RAM, GPU, driver version, and hardware encoder/decoder identity;
- AC/battery state and active Windows power plan;
- display resolution, refresh rate, scale (100/125/150/200%), and 1200x760 Clipline window size;
- foreground/background applications, system-wide idle CPU, antivirus scan/exclusion state, and
  remote-session status;
- exact Git commit, executable SHA-256, benchmark probe, renderer, fixture hashes, and WebView2 or
  Slint runtime/version/features.

For CPU comparisons, reject a run with active updates/scans, thermal throttling, or sustained
background CPU. Alternate Tauri and Slint run order instead of measuring every run of one frontend
first. Use at least three clean launches per matched scenario.

## Scenarios and readiness

`scripts/measure-frontend-baseline.ps1` accepts:

| Scenario | Required semantic state |
|---|---|
| `autostart-tray` | Tray built, autostart hide completed, native main window absent. |
| `library-50`, `library-500`, `library-2000` | Exact clip count indexed and Local Library cards rendered from hard-linked corpus bytes. |
| `settings` | Foreground bootstrap complete, settings model loaded, overlay visible/interactable. |
| `review-idle` | Two-track fixture open, duration known/current data decoded, transport paused. |
| `review-playing` | Same fixture open and media time advancing. |
| `scrub-storm` | Review open/paused; repeated distant seeks continue through the steady phase. |
| `close-to-tray` | Real native close request reaches the configured tray lifecycle state. |
| `reveal-close-100` | 100 real secondary-instance reveals and native close-to-tray cycles complete. |

The driver writes a frontend-neutral JSON-lines marker:

```json
{"schemaVersion":1,"kind":"ready","timestampUtc":"...","detail":"..."}
```

`kind:error` fails the run. The built-in Tauri adapter maps DOM/CDP and diagnostic-log observations
to this marker. A Slint adapter is an external PowerShell script invoked with `-ContextPath`; it must
produce the same marker file described in that context. The sampler/output schema does not depend
on CDP.

## Run commands

Example short smoke (not a publishable five-minute baseline):

```powershell
./scripts/measure-frontend-baseline.ps1 `
  -Exe target/benchmark/clipline-app.exe `
  -Frontend tauri `
  -Renderer webview2 `
  -Scenario library-50 `
  -FixturesDir fixtures/playback `
  -WarmupSeconds 5 `
  -SteadySeconds 15 `
  -OutputDirectory artifacts/slint-baseline-smoke
```

Gate run:

```powershell
./scripts/measure-frontend-baseline.ps1 `
  -Exe target/benchmark/clipline-app.exe `
  -Frontend tauri `
  -Renderer webview2 `
  -Scenario review-playing `
  -FixturesDir fixtures/playback `
  -WarmupSeconds 30 `
  -SteadySeconds 300 `
  -SampleIntervalMs 1000 `
  -OutputDirectory artifacts/slint-baseline
```

Run every scenario three times for Tauri and again for each Slint renderer under consideration.
Use a 75-100 ms interval for scrub-storm peak work; one-second sampling is the default steady-state
interval.

## Evidence and acceptance

Each successful run writes:

- `<run>.raw.csv`: stable per-process/process-tree columns;
- `<run>.metadata.json`: machine/build/corpus/profile/timing/readiness data and p50/p95 summaries;
- `profiles/<run>/`: isolated settings, media links, WebView data, driver context/markers, and logs.

Before accepting a run, verify zero child-read failures in the steady window, no unexpected FFmpeg
child, unchanged corpus hashes, intended foreground/tray state, and no driver error after readiness.
Compare p50 private working set and private commit against the numeric gates in the replacement
program. Keep raw evidence; never transcribe only the rounded headline.

No matched measurements have been collected on this branch yet. The current installed Clipline was
left running, and the harness correctly refuses to disturb or measure it.
