# Windows NSIS Uninstaller Implementation Plan

**Goal:** Ship an interactive Tauri NSIS uninstall flow that always removes Clipline-owned application residue, preserves recordings by default, and deletes only verified Clipline-owned recordings after explicit consent.

**Architecture:** NSIS owns only process shutdown, the recordings prompt, helper invocation while `Clipline.exe` still exists, and removal of known leftover install-directory children. A pre-runtime `--uninstall-cleanup` command builds a conservative cleanup plan from injected paths and persisted settings, then executes it best-effort. Managed-media ownership and deletion stay in `clipline-storage` so the uninstaller and storage logic share one definition.

**Tech Stack:** Rust, Tauri 2, Windows Credential Manager and registry APIs through the existing `windows-sys` dependency, NSIS installer hooks, `clipline-test-utils::TestDir`.

---

### Task 1: Add Managed-Media Deletion Tests

**Files:**
- Modify: `crates/clipline-storage/src/lib.rs`

- [ ] Add failing tests for a public managed-media deletion helper covering marked clips, legacy Clipline names, `.mp4.recording`, all owned sidecars, the session manifest, unmarked MP4 preservation, empty session cleanup, and symlink/reparse-point avoidance.
- [ ] Run the focused storage tests and confirm they fail before implementation.
- [ ] Implement the smallest public helper by reusing the existing ownership and media-directory traversal logic.
- [ ] Run the focused storage tests and confirm they pass.

### Task 2: Lock The Installer Contract

**Files:**
- Modify: `apps/clipline-app/tests/ui_contract.rs`

- [ ] Add a Linux-safe failing contract test requiring `windows/hooks.nsh`, `--uninstall-cleanup`, the opt-in `--delete-recordings` argument, silent/update bypasses, and `bundle.windows.nsis.installerHooks` registration.
- [ ] Confirm the focused contract test fails before implementation.

### Task 3: Model Cleanup Without Real AppData

**Files:**
- Create: `apps/clipline-app/src/uninstall.rs`
- Modify: `apps/clipline-app/src/main.rs`

- [ ] Add failing tests around an injected `CleanupLayout` using `clipline-test-utils::TestDir`.
- [ ] Cover preservation of recordings by default, opted-in managed-media deletion, named local-cache children only, permanent exclusion of shared `Microsoft\\EdgeWebView`, replay-cache/media overlap, and refusal of dangerous root paths.
- [ ] Read `settings.json`, falling back to `settings.json.bak`, before scheduling the config tree for deletion.
- [ ] Implement a conservative plan containing only known Clipline paths, credential targets, and autostart value names.
- [ ] Confirm the focused planner tests pass without reading or modifying real AppData.

### Task 4: Execute Best-Effort Residue Cleanup

**Files:**
- Modify: `apps/clipline-app/src/uninstall.rs`
- Modify: `apps/clipline-app/Cargo.toml`

- [ ] Add test-injected credential and autostart callbacks so Linux-safe tests can verify planned side effects.
- [ ] Implement best-effort file/directory removal, managed-media cleanup, known credential deletion plus Windows Credential Manager prefix enumeration, and removal of both Clipline autostart value names.
- [ ] Add only the required `Win32_System_Registry` feature to the existing `windows-sys` dependency.
- [ ] Keep unsafe Windows API calls behind safe wrappers and keep the helper successful on partial cleanup failure.

### Task 5: Intercept The Cleanup CLI Before Runtime Startup

**Files:**
- Modify: `apps/clipline-app/src/main.rs`
- Modify: `apps/clipline-app/src/uninstall.rs`

- [ ] Add a failing argument-routing test showing `--uninstall-cleanup` selects cleanup rather than normal app startup and recognizes optional `--delete-recordings`.
- [ ] Intercept the command before elevation waiting, diagnostics initialization, or `app::run()`.
- [ ] Return process success after best-effort cleanup, including partial failures.

### Task 6: Add Tauri NSIS Hooks

**Files:**
- Create: `apps/clipline-app/windows/hooks.nsh`
- Modify: `apps/clipline-app/tauri.conf.json`
- Modify: `apps/clipline-app/tests/ui_contract.rs`

- [ ] Confirm Tauri 2's generated NSIS variables and command-line flags from the installed bundler template.
- [ ] Register `windows/hooks.nsh` in the default NSIS configuration so both normal and standalone configurations inherit it.
- [ ] In `NSIS_HOOK_PREUNINSTALL`, skip silent/updater runs, terminate `Clipline.exe`, prompt once for recording deletion, and invoke the helper without a console.
- [ ] In `NSIS_HOOK_POSTUNINSTALL`, remove only known empty/leftover install-directory children and never recursively remove `$INSTDIR` as user data.
- [ ] Run the focused UI contract test and confirm it passes.

### Task 7: Verify Cleanup Behavior

**Files:**
- No source edits unless tests expose a defect.

- [ ] Run focused `clipline-storage` managed-media tests.
- [ ] Run focused `clipline-app` uninstall tests.
- [ ] Run the NSIS UI contract test.
- [ ] Review the diff for any path capable of targeting `%LOCALAPPDATA%\\Microsoft\\EdgeWebView`, the entire `%LOCALAPPDATA%\\Clipline` root, an arbitrary custom-media file, or `paseo.json`.

### Task 8: Update Project Handoff

**Files:**
- Modify: `handoff.md`

- [ ] Record the cleanup CLI, NSIS behavior, recording opt-in rule, shared WebView2 exclusion, tests, and remaining manual uninstall smoke test.

### Task 9: Run Quality Gates

**Files:**
- No source edits unless verification fails.

- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clean -p clipline-app` to avoid warm-cache Clippy false confidence.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Stop any existing Clipline process and run `cargo run -p clipline-app` for the user test handoff.

### Task 10: Publish And Merge

**Files:**
- Commit only the planned source, test, config, plan, and handoff files.

- [ ] Inspect staged scope and verify `paseo.json` remains untracked.
- [ ] Commit logical changes with conventional commit messages.
- [ ] Push `feat/nsis-uninstaller` and open a PR targeting `develop`.
- [ ] Confirm required CI checks pass, merge the PR on GitHub, then delete the remote and local feature branch without checking out `develop` in this worktree.

---

## Safety Invariants

- [ ] Interactive removal is the only flow that prompts or deletes recordings; silent and updater flows skip cleanup.
- [ ] `%LOCALAPPDATA%\\Microsoft\\EdgeWebView` is never planned or removed.
- [ ] `%LOCALAPPDATA%\\Clipline` is never recursively removed by the helper.
- [ ] Custom media roots retain unrelated files and remain present even when emptied of Clipline-owned files.
- [ ] The default `Videos\\Clipline` directory is removed only when opted-in recording deletion leaves it empty.
- [ ] Directory traversal does not follow symlinks or reparse points.
- [ ] Cleanup errors never block NSIS from continuing.
