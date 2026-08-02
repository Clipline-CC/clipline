# Slint Native Shell and Lifecycle Implementation Plan

> **For agentic workers:** Execute one task at a time with failing tests first. Commit this plan
> before implementation. Keep checkboxes unticked by repository convention.

**Program milestone:** Milestone 6 of `2026-08-01-slint-frontend-replacement.md`.

**Goal:** Replace Clipline's Tauri-specific tray, global-shortcut, autostart, single-instance, and
updater dependencies with tested first-party contracts and safe Windows wrappers that both the
shipping Tauri app and the Slint candidate can use. Prove a tray-only Slint process can own durable
application state, lazily create/drop its window, activate from a same-user secondary process, and
be packaged internally without weakening updater or installer security.

**Non-goals:** Cutting over the shipping UI, porting Library/Cloud/Settings/Review surfaces,
changing settings/media/credential identities, publishing a Slint installer, removing Tauri core,
changing update keys/channels, linking FFmpeg, or claiming Win10/Win11/real-GPU/manual gates that
were not run.

**Baseline:** branch `agent/slint-frontend-replacement-plan` after Milestone 5 closeout `7f83c32`.
The user's installed Clipline remains open. Never stop it, mutate its Run entry, install an internal
candidate, or write to its settings/credential profile without explicit permission.

## Architecture and hard contracts

Add a small framework-neutral/Windows-gated `clipline-shell` workspace crate. Neutral modules own
launch parsing, shell commands, lifecycle policy, bounds, and transaction decisions. Every Win32
call and every unsafe block lives under `clipline-shell/src/windows/` behind safe wrappers. The
crate must not import Tauri, Slint, WebView2, recorder, cloud, or application settings types.

Add a separate `clipline-updater` crate. Manifest parsing, version/channel/variant policy,
streamed-download bounds, signature verification, and cancellation remain UI-neutral. Only passive
installer launch is Windows-gated. This keeps HTTP/signature dependencies out of the tray/hotkey
shell and makes tampered-input tests portable.

The shipping Tauri app remains the production binary throughout this milestone. Replace its four
Tauri plugins (global shortcut, autostart, single instance, updater) one vertical slice at a time
with the shared implementations, preserving exact command payloads and settings transaction
ordering. Tauri's own tray/window remains the shipping presentation adapter until Milestone 10.

The Slint candidate owns one long-lived shell runtime and `DesktopController`; a `MainWindow` is an
optional UI-thread attachment. Tray-only/autostart must instantiate `SystemTrayIcon`, the shell
runtime, recorder/hotkey/activation services, and no Slint window adapter or renderer. Closing to
tray must first publish durable lifecycle state, stop/release microphone/playback/window-owned
resources, detach the weak UI adapter, then drop `MainWindow`. Reopen creates a new component and
applies one current controller snapshot.

Pinned bounds and policies:

- shell command inbox: 32 entries, nonblocking producers, one reserved `Quit`, `Open` and Save
  Replay coalescing without crossing a quit/update-install barrier;
- activation message: one length-prefixed JSON object, at most 4 KiB, local machine only, bounded
  accept/read deadlines, no inherited handles, same-user token verification before dispatch;
- autostart value: exact quoted current executable plus `--autostart`, HKCU only, read-back
  verification, rollback to the exact prior value on failure, and no mutation in debug/benchmark;
- updater manifest: 256 KiB; installer: 512 MiB; HTTPS only; approved channel/variant endpoint;
  streamed owned temp; existing embedded minisign public key; signature verification before launch;
- update/install commands: one active operation, cancellation token, bounded 20-second metadata
  request, no silent channel/variant crossing, no downgrade or same-version install;
- lazy window: zero window/renderer creation in autostart tray until activation; one window at a
  time; component callbacks and posted closures capture only weak handles;
- reveal/close soak: 100 exact cycles, stale generations rejected, no more than 10 MiB private
  working-set growth under the program's matched sampler;
- package candidate: current-user install scope, `io.clipline.app`, Clipline publisher/product
  names, icons, FFmpeg resource and notices, regular/standalone variant identity, and no WebView2 in
  the native candidate payload.

## Task 1: Freeze the neutral shell contract and bounded command port

**Files**

- Modify: `Cargo.toml`
- Create: `crates/clipline-shell/Cargo.toml`
- Create: `crates/clipline-shell/src/lib.rs`
- Create: `crates/clipline-shell/src/contract.rs`
- Create: `crates/clipline-shell/src/channel.rs`
- Create: `crates/clipline-shell/tests/contract.rs`
- Create: `crates/clipline-shell/tests/channel.rs`
- Modify: `apps/clipline-app/tests/repository_security.rs`

**Test first**

- `ShellLaunch` parses normal, `--autostart`, updater handoff, and exact elevated-parent arguments;
  malformed/duplicate/oversized arguments fail without partially accepting a launch mode.
- `ShellCommand::{Open, SaveReplay, OpenDiagnostics, Quit, CheckUpdates, InstallUpdate}` is owned,
  serializable where needed, and has explicit durable/coalescable semantics.
- The 32-entry port reserves one terminal slot, never blocks a producer, coalesces Open/Save only
  within the current barrier epoch, reports Full/Disconnected distinctly, and never wraps sequence.
- `WindowPolicy` deterministically maps normal/autostart launch, close, minimize, reveal, taskbar,
  and explicit quit to lifecycle effects without framework types.
- Repository security rejects Tauri/Slint/Win32 imports in neutral modules and unsafe outside
  `src/windows/`.

**Implement to green**

- Keep shell decisions data-only. Effects name application work but do not call recorder/UI APIs.
- Add typed checked `ShellGeneration`/`ShellSequence`; no saturating or wrapping identity counters.
- Match capture/playback's reviewed `windows` 0.62 line only in the Windows target dependency.

## Task 2: Move hotkey grammar out of the Tauri plugin

**Files**

- Create: `crates/clipline-shell/src/hotkey.rs`
- Create: `crates/clipline-shell/tests/hotkey.rs`
- Modify: `apps/clipline-app/src/settings/hotkey.rs`
- Modify: `apps/clipline-app/src/settings/mod.rs`
- Modify: existing settings/hotkey tests

**Test first**

- Port every accepted/rejected current vector: F1-F11/F13-F24, keyboard punctuation/navigation,
  Middle/Mouse4/Mouse5, modifier normalization, duplicate parts, F12, Alt+F4, Alt+Tab,
  Ctrl+Alt+Delete, unmodified keyboard keys, and Escape.
- The neutral spec maps to stable virtual-key/modifier values without importing
  `tauri_plugin_global_shortcut::Shortcut`.
- Existing serialized settings and normalized labels are byte-for-byte unchanged.

**Implement to green**

- Make the app settings module a compatibility re-export/adapter over `clipline-shell::hotkey`.
- Do not change the saved hotkey format or supported combinations.

## Task 3: Add one transactional Windows hotkey service

**Files**

- Create: `crates/clipline-shell/src/windows/mod.rs`
- Create: `crates/clipline-shell/src/windows/hotkey.rs`
- Create: `crates/clipline-shell/tests/windows_hotkey.rs`
- Modify: `apps/clipline-app/src/hotkeys.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/Cargo.toml`

**Test first**

- RegisterHotKey-backed keyboard chords and low-level keyboard/mouse fallback deliver one logical
  trigger per key-down episode and do not double-fire when both paths observe the chord.
- Replacement registers/validates the full candidate set, rolls every change back on one failure,
  and leaves the prior active set exact. Removing the secondary chord is equally transactional.
- Hook threads have bounded startup, explicit stop/join, and idempotent teardown; callbacks enqueue
  shell commands and never run recorder work on a Win32 hook thread.
- Elevated foreground-game diagnostics and missing-registration warning strings/timing remain
  unchanged in the Tauri adapter.

**Implement to green**

- Move raw hooks and registration into the safe shell wrapper; retain a small app callback adapter.
- Remove `tauri-plugin-global-shortcut` only after all Tauri hotkey/settings/rollback tests pass.
- Keep debouncing at the application Save Replay boundary as defense against distinct trigger
  sources, not as a substitute for correct registration state.

## Task 4: Replace autostart with an HKCU transaction

**Files**

- Create: `crates/clipline-shell/src/windows/autostart.rs`
- Create: `crates/clipline-shell/tests/windows_autostart.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/Cargo.toml`
- Modify: settings transaction tests

**Test first**

- Command quoting round-trips executable paths containing spaces/quotes and always adds exactly one
  `--autostart` argument.
- Read/enable/disable are per-user and value-name scoped. Candidate write, read-back verification,
  and rollback preserve an absent, valid, or foreign prior value exactly.
- Debug and benchmark builds return the persisted preference without touching the registry.
- A settings save failure restores hotkeys, tray label, registry value, persisted settings, and
  recorder runtime in the existing transaction order.

**Implement to green**

- Use `RegGetValueW`/`RegSetValueExW`/`RegDeleteValueW` through one RAII key wrapper under
  `windows/`; never shell out to `reg.exe` for mutation.
- Remove `tauri-plugin-autostart` after the shipping app uses this adapter.

## Task 5: Add an authenticated same-user single-instance activation channel

**Files**

- Create: `crates/clipline-shell/src/activation.rs`
- Create: `crates/clipline-shell/src/windows/activation.rs`
- Create: `crates/clipline-shell/tests/activation.rs`
- Create: `crates/clipline-shell/tests/windows_activation.rs`
- Modify: `apps/clipline-app/src/main.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/Cargo.toml`

**Test first**

- The instance name is derived from product identity plus the current user SID, not username text;
  a recycled PID or another user's token cannot authenticate.
- The primary acquires ownership before recorder/services start. A secondary sends one bounded
  activation and exits; non-autostart reveals, secondary autostart is an acknowledged no-op.
- The pipe rejects remote clients, payloads above 4 KiB, invalid UTF-8/JSON, duplicate fields,
  unsupported schema/commands, incomplete length prefixes, timeout, and peer-token mismatch.
- Activation arriving before UI attachment remains in the 32-entry shell port; activation after
  tray-only startup reveals the lazily created window exactly once.
- Listener stop closes the pipe, joins the thread, and does not retain the executable or settings.

**Implement to green**

- Use one named per-user mutex plus a local named pipe. Verify the client process token SID against
  the primary SID before parsing/dispatching the message; `PIPE_REJECT_REMOTE_CLIENTS` is additive,
  not sufficient authentication by itself.
- Acquire or activate before constructing the Tauri builder. Remove
  `tauri-plugin-single-instance` only after secondary-launch tests are green.

## Task 6: Consolidate shared safe Windows shell services

**Files**

- Create: `crates/clipline-shell/src/windows/process.rs`
- Create: `crates/clipline-shell/src/windows/shell_execute.rs`
- Create: `crates/clipline-shell/src/windows/clipboard.rs`
- Create: `crates/clipline-shell/src/windows/credential.rs`
- Modify: `apps/clipline-app/src/windows/mod.rs`
- Modify: `apps/clipline-app/src/windows/credential_store.rs`
- Modify: `apps/clipline-app/src/library.rs`
- Modify: `apps/clipline-app/src/cloud.rs`
- Modify: `apps/clipline-app/src/osu_api.rs`
- Modify: `apps/clipline-app/src/app/support.rs`

**Test first**

- Elevation parent handoff retains PID+creation-time ABA defense and exact argument parsing.
- Shell open validates embedded NULs and result codes; Explorer reveal uses argument-safe Win32
  shell APIs rather than concatenated command lines.
- File clipboard ownership closes every opened path, retries only bounded transient contention,
  transfers global memory exactly once, and preserves verbatim-UNC normalization.
- Credential CRUD preserves existing target/value labels and secret bytes without logging them.
- Repository security confirms all moved unsafe is under `clipline-shell/src/windows/` and the app
  reaches only safe functions.

**Implement to green**

- Preserve current folder pickers (`rfd`) and application validation. This task moves shell
  mechanics, not use-case policy.
- Keep credential targets, media paths, and clipboard/share export behavior unchanged.

## Task 7: Build the framework-neutral signed updater

**Files**

- Modify: `Cargo.toml`
- Create: `crates/clipline-updater/Cargo.toml`
- Create: `crates/clipline-updater/src/lib.rs`
- Create: `crates/clipline-updater/src/manifest.rs`
- Create: `crates/clipline-updater/src/download.rs`
- Create: `crates/clipline-updater/src/windows.rs`
- Create: `crates/clipline-updater/tests/manifest.rs`
- Create: `crates/clipline-updater/tests/update_flow.rs`
- Create: small known-good/bad signature fixtures under `crates/clipline-updater/tests/fixtures/`
- Modify: `apps/clipline-app/src/updates.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/Cargo.toml`

**Test first**

- Parse current regular/standalone manifests under 256 KiB and reject unknown platform, duplicate
  keys, non-HTTPS URL, wrong channel/variant, invalid semver/date, same/downgrade version, and
  redirects outside the approved release-download policy.
- Stream an installer to an invocation-owned temp with a 512 MiB limit, cancellation, exact content
  length/hash telemetry, no overwrite, and cleanup after every failure.
- Verify the existing manifest signature over exact installer bytes with the existing embedded
  public key. Known-good passes; one-byte tamper, wrong key/signature, renamed/crossed variant, and
  truncated file fail before launch.
- Passive installer launch happens only after verification and returns a typed handoff. Offline,
  timeout, cancellation, process-launch failure, and service-stop failure leave Clipline running.
- Only after launch succeeds does the app stop mic/recorder, publish durable shutdown state, and
  exit. Tests pin this ordering.

**Implement to green**

- Reuse the currently resolved `minisign-verify` 0.2.5 algorithm/API after recording its license
  and dependency review; do not invent a new signature format or key.
- Use the workspace rustls `reqwest` path with redirects and sizes explicitly constrained.
- Remove `tauri-plugin-updater` only when legacy command JSON and release fixtures are green.

## Task 8: Make the Slint shell tray-first and its window truly lazy

**Files**

- Modify: `apps/clipline-slint-spike/ui/app.slint`
- Create: `apps/clipline-slint-spike/src/shell.rs`
- Modify: `apps/clipline-slint-spike/src/desktop.rs`
- Modify: `apps/clipline-slint-spike/src/lib.rs`
- Modify: `apps/clipline-slint-spike/src/main.rs`
- Create: `apps/clipline-slint-spike/tests/shell.rs`
- Modify: `apps/clipline-slint-spike/tests/spike_contract.rs`
- Modify: `scripts/drive-slint-spike.ps1`
- Modify: `scripts/measure-frontend-baseline.ps1`

**Test first**

- `--autostart` constructs tray/runtime/activation/hotkeys but no `CliplineSpike`, winit window,
  renderer, playback session, video host, posters, or visible models before Open.
- Normal launch and tray/activation Open create at most one UI-thread window, attach a weak desktop
  projection, and rebuild current controller state before accepting callbacks.
- Close-to-tray publishes background first, stops mic/playback, releases host/file/audio/model
  resources, detaches the weak adapter, drops the component, and keeps recorder/hotkeys/tray/event
  consumer alive. Explicit Quit orders every service stop and exits the event loop once.
- The tray and window use explicit synchronized label/state values; they share no Slint global.
- Open/Save/Diagnostics/Quit and left-button-release behavior match the parity ledger.
- One hundred open/close cycles reject stale closures and report exact created/dropped counts,
  threads/handles, and no retained presentation resources.

**Implement to green**

- Refactor the current Slint adapter so its `DesktopController` and neutral event consumer outlive
  any component attachment. `attach(Weak<CliplineSpike>)` posts one snapshot; `detach` invalidates
  all previously posted revisions without stopping the controller.
- Keep M4 playback and D3D host window-owned; do not retain them in tray state.
- Extend the benchmark adapter with semantic `trayReady`, `windowCreated`, `windowDropped`, and
  lifecycle counters. Formal memory claims still require quiet matched runs.

## Task 9: Put the shipping Tauri shell on the shared implementations

**Files**

- Modify: `apps/clipline-app/src/main.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/src/updates.rs`
- Modify: `apps/clipline-app/Cargo.toml`
- Modify: `apps/clipline-app/tests/desktop_contract.rs`
- Modify: `apps/clipline-app/tests/ui_contract.rs`
- Modify: `apps/clipline-app/tests/slint_migration_contract.rs`
- Modify: `apps/clipline-app/tests/repository_security.rs`

**Test first**

- Existing 60-command JSON, tray labels/actions, launch behavior, settings rollback, update dialog,
  diagnostics, elevation, and window lifecycle contracts remain exact.
- Source/dependency tests reject the four removed Tauri plugins and direct Win32 shell work outside
  shared safe wrappers.
- A native-shell failure is typed, logged, visible where previously visible, and never silently
  starts a second recorder or mutates settings partially.
- Debug benchmark probe still proves no autostart mutation.

**Implement to green**

- Tauri adapts AppHandle/window calls only. Shell ownership, commands, hotkeys, autostart,
  activation, updater policy, and Windows operations live in shared crates.
- Keep Tauri tray/window/core and WebView2 dependencies for rollback until Milestone 10.

## Task 10: Prove a non-distributed native NSIS candidate

**Files**

- Create: `packaging/slint/installer.nsi`
- Create: `packaging/slint/installer-shared.nsh`
- Create: `scripts/build-slint-installer.ps1`
- Create: `scripts/test-slint-installer-tools.ps1`
- Create: `docs/slint/native-shell-package-protocol.md`
- Modify: `.github/workflows/ci.yml`
- Modify: `apps/clipline-app/tests/slint_migration_contract.rs`
- Modify: `docs/slint/parity-ledger.md`

**Test first**

- Script tests pin current-user scope, `io.clipline.app`, product/publisher/version/icons, regular
  and standalone variant IDs/names, shortcuts/uninstall metadata, upgrade/downgrade policy, passive
  mode, and exact FFmpeg/notices resources.
- Staging refuses dirty/missing/oversized/substituted FFmpeg, missing license/attribution, wrong
  executable hash/version/variant, pre-existing output, or any WebView2 payload.
- The produced installer can be extracted without execution; staged file hashes match and no
  Tauri/WebView asset is present.
- Updater fixtures verify regular and standalone manifests cannot cross-install.

**Implement to green**

- Use an explicit first-party NSIS script and a pinned/discovered `makensis` path; do not hide
  packaging behind Tauri bundling or auto-download tools during CI.
- Name the binary/artifact as an internal Slint candidate and do not publish it. Installation,
  upgrade, uninstall, and signed update smokes run only in isolated Windows 10/11 VMs with explicit
  operator approval; record them pending otherwise.

## Task 11: Validate and close Milestone 6

**Files**

- Modify: `docs/slint/parity-ledger.md`
- Modify: `docs/slint/native-shell-package-protocol.md`
- Modify: `handoff.md`

**Validation**

- Mark only implemented M6 rows. Keep installer/update/manual rows `in_progress` until isolated
  VM/signing tests are accepted; never convert compile/unit evidence into an installed-pass claim.
- Run all new shell/updater tests on neutral CI and Windows device/integration tests locally.
- Run standalone Slint tests and fresh-cache strict Clippy.
- Run `cargo test --workspace` with documented device skips, fresh-cache changed-crate Clippy, then
  strict workspace Clippy.
- Run UI/migration/repository-security and PowerShell helper tests.
- Run debug/benchmark probes proving no registry mutation.
- Do not launch the shipping Tauri build while an indistinguishable installed process is open.
- Run tray-only/open/close/activation diagnostics without touching the installed profile; formal
  memory/100-cycle gates require accepted sampler envelopes.
- Push each logical commit and require Ubuntu/Windows/RustSec CI green before calling M6 complete.

## Stop conditions

- Stop if primary ownership is acquired after recorder/services start, a peer is not same-user
  authenticated, activation/update input is unbounded, or a hook/pipe thread cannot stop/join.
- Stop if debug/benchmark code can modify the installed Run entry, or autostart rollback cannot
  restore an exact pre-existing foreign value.
- Stop if any updater path launches before exact signature verification, follows an unapproved
  URL/variant, accepts a downgrade, overwrites a caller file, or leaves an unbounded temp.
- Stop if tray-only Slint constructs a window/renderer, or close-to-tray retains component,
  playback, file, audio, D3D, poster, or visible-model ownership.
- Stop if shared neutral code imports Tauri/Slint or unsafe escapes `src/windows/`.
- Keep Tauri shipping and rollback-capable; do not publish/install the native candidate during this
  milestone.
