# Non-destructive storage quota implementation plan

## Goal

Replace Clipline's oldest-first saved-clip garbage collection with a hard,
recoverable recording lock. Clipline must never delete a non-empty saved clip
without an explicit user action.

When the configured saved-media quota has no room for a replay or full-session
recording, Clipline will stop the recorder, keep all existing media, display a
durable quota-full dialog, and remain blocked until the quota is increased or
enough media is removed.

## Product rules

- Saved clips and non-empty unfinished recordings are user data. Automatic
  cleanup may only remove zero-byte placeholders and ephemeral replay-cache
  files.
- A configured quota of `0` remains unlimited.
- At startup, a library already at or above quota blocks recorder
  initialization before capture hardware is opened.
- Before saving a replay, Clipline measures the encoded replay window and
  reserves conservative muxing overhead. If it will not fit, no output file is
  created and the recorder enters the quota-full state.
- Full-session recording reserves finalization headroom and checks usage while
  recording. It stops and finalizes the current session before that headroom is
  consumed.
- If a completed output unexpectedly crosses the limit, Clipline preserves it
  and enters the quota-full state. It never makes room by deleting another
  clip.
- The quota-full state disables Save Replay and recording controls. Hotkeys,
  tray actions, and commands cannot bypass it.
- The dialog can be dismissed so the user can manage their Library or
  Settings, but the lock remains visible and active. Repeated save/start
  attempts show it again.
- Increasing the quota, changing the media folder, or deleting clips in the
  Library rechecks the block. Recording resumes automatically only if it had
  been desired before the quota lock.

## Implementation sequence

1. Add storage tests proving quota inspection never mutates the media tree,
   then delete the saved-clip quota-GC API and its destructive tests.
2. Add replay-pipeline tests for measuring the selected encoded window, then
   expose the byte count needed for save preflight.
3. Add service tests for quota capacity calculations, startup blocking,
   replay-save preflight, full-session headroom, preservation of successful
   outputs, and preservation of non-empty failed/short session files.
4. Add a serializable `storage_quota_full` service event. Resolve and recover
   the media directory before hardware setup, block already-full starts, check
   replay saves before file creation, and stop/finalize full sessions at the
   safety threshold.
5. Store quota lock state in `RuntimeState`. Gate every recorder spawn and save
   entry point, replay the durable event after frontend readiness, and add a
   recheck command that clears the lock and conditionally restarts recording.
6. Add UI contract tests for the modal, safe quota copy, listeners, disabled
   controls, and removal of the old cleanup toast. Implement the accessible
   dialog with actions to manage clips, open Storage settings, open the media
   folder, and check again.
7. Update `ddoc.md` and `handoff.md` to make explicit deletion the only
   saved-media deletion policy.
8. Run workspace tests and fresh-cache Clippy, commit the implementation,
   rebuild, and relaunch Clipline for manual verification.

## Manual verification

- Put the library at its configured limit and launch Clipline: the dialog opens
  and capture hardware does not start.
- Attempt Save Replay when the selected replay would cross the quota: no file
  appears, the recorder stops, and existing clips remain byte-for-byte intact.
- Let a full session approach the limit: it finalizes safely, remains in the
  Library, and the recorder locks.
- Dismiss the dialog and try the rail, F6, and tray Save Replay actions: none
  records or saves, and the dialog returns.
- Delete enough clips explicitly or raise the quota: Check again clears the
  warning and resumes a previously desired recorder.
- Confirm that quitting or a finalization error leaves every non-empty
  `.mp4.recording` file available for recovery.
