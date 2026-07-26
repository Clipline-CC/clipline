# PR #107 second-review follow-ups

**Goal:** Land the non-blocking robustness findings from the second review without weakening the
upload-source race guarantees or adding unbounded work.

**Base:** PR #107 (`pr-memory-gaps`) stacked on PR #106 (`memory-optimization`).

## Task 1: Keep measurement sampling resilient to child exit

**Files:**
- Modify: `scripts/measure-save-replay-memory.ps1`

- [ ] Preserve loud failures when the root process or EX2 counters are unavailable.
- [ ] Treat a child process exiting between tree discovery and sampling as a benign race.
- [ ] Parse and exercise both the root-failure and child-skip paths.

## Task 2: Pin the duplicated clip-path relation

**Files:**
- Modify: `apps/clipline-app/tests/gallery_window_core.rs`
- Modify: `apps/clipline-app/ui/gallery-window-core.js`

- [ ] Load `PlayerCore` and `GalleryWindowCore` into one Boa context.
- [ ] Cross-check representative Windows, UNC, device-prefix, non-Windows, empty, and mixed pairs.
- [ ] Correct the pagination comment to match production identity semantics.

## Task 3: Remove the timeout fixture's shell dependency

**Files:**
- Modify: `apps/clipline-app/src/poster.rs`

- [ ] Replace the PATH-resolved PowerShell fixture with a self-contained child process.
- [ ] Keep the kill-and-reap assertion unchanged.

## Task 4: Evaluate upload mutex contention

**Files:**
- Modify if warranted: `apps/clipline-app/src/cloud_upload.rs`

- [ ] Preserve serialized file-identity registration and mutation checks.
- [ ] Keep the zero-upload fast path free of file opens.
- [ ] Move unavoidable blocking acquisition work off the async executor if it materially improves
      behavior without weakening the TOCTOU guarantee.

## Task 5: Validate and publish

- [ ] Run focused PowerShell, UI, poster, and upload tests.
- [ ] Run `cargo test --workspace`.
- [ ] Run fresh-cache `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run formatting, JavaScript syntax, PowerShell parser, and `git diff --check`.
- [ ] Commit and push the stacked branch; verify Windows and Ubuntu CI.
- [ ] Leave GitHub review threads untouched unless explicitly authorized.
