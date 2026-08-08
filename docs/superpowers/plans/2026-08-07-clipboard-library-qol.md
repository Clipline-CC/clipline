# Clipboard And Library Feedback Plan

**Goal:** Make clipboard exports safe to abandon, expose both clipboard actions in the library,
and make cloud-upload activity visible without opening a clip.

## Product decisions

- A normal toolbar copy keeps producing a shareable clip for videos up to and including five
  minutes. For longer videos it copies the original media file immediately.
- Shift-click keeps its existing explicit `copy original` behavior.
- The library context menu exposes both unambiguous actions: `Copy to clipboard` copies the
  original media file, while `Copy shareable clip` always requests the compatible export.
- Hiding or quitting Clipline cancels an in-progress shareable export and terminates its FFmpeg
  child process. Temporary export files continue to use the existing cleanup path.
- A local clip shows a spinner while its cloud record is queued, uploading, retrying, or
  processing. App-wide notices announce upload start and successful completion.

## Minimal implementation

Use one process-owned clipboard-export generation token. Each new export captures the current
generation; cancellation advances it. The existing FFmpeg polling loop checks the token, kills and
reaps the child, and returns through the normal error cleanup. This avoids a second process manager
or a frontend-only cancellation flag that cannot stop backend work.

Keep all copy actions on the existing `copy_clip_to_clipboard` command. The frontend only chooses
whether `original` is true and supplies the selected library clip.

## Plan-driven implementation

### Task 1: Lock the behavior with failing tests

- [ ] Add a Rust unit test proving cancellation is observable by an active clipboard-export job
  and that a later job is not born cancelled.
- [ ] Update UI-contract tests for the five-minute toolbar threshold and the two local-library
  clipboard actions.
- [ ] Add UI-contract coverage for upload-start/upload-finish notices and the library upload
  activity indicator.

### Task 2: Cancel abandoned shareable exports

- [ ] Manage the clipboard-export cancellation state with the Tauri application.
- [ ] Cancel current work whenever the main window is sent to the background or the application
  exits.
- [ ] Thread the active job through the existing shareable export path and terminate FFmpeg when
  cancellation is observed.
- [ ] Check cancellation immediately before placing the resulting file on the Windows clipboard.

### Task 3: Improve clipboard actions

- [ ] Make toolbar copy use the original media file when `duration_s` exceeds 300 seconds.
- [ ] Add `Copy to clipboard` and `Copy shareable clip` to the local-clip context menu, while
  keeping them hidden for cloud-only and game-launch entries.
- [ ] Reuse the existing copy command and existing selected-audio behavior for shareable exports.

### Task 4: Surface upload activity

- [ ] Render a compact spinner at the left of a local clip title while its cloud record is busy.
- [ ] Announce upload start and successful completion through the app-wide notice area.
- [ ] Preserve the existing progress events, deck status, retry behavior, and post-upload refresh.

### Task 5: Verify and hand off

- [ ] Run focused tests while iterating.
- [ ] Run `cargo test --workspace`.
- [ ] Run a clean-cache `cargo clippy --workspace --all-targets -- -D warnings` for changed crates.
- [ ] Update `handoff.md`, rebuild, and relaunch Clipline for manual testing.
