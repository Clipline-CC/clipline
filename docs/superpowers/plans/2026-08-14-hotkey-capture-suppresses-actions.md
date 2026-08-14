# Hotkey Capture Suppresses Live Actions

**Goal:** Rebinding a key in Settings must not fire the currently registered action. Swapping Save
Replay onto another key, then assigning the old key to Bookmark, should record the bind — not save
a replay.

## Why this happens

Hotkeys stay live from the **last saved** settings until Save. The Settings fields are a draft.

Save Replay F-keys are registered with the OS (`RegisterHotKey` via Tauri) **and** matched by the
low-level hook. Bookmark and recording binds are hook-only.

So this sequence is sufficient:

1. Replay is bound to the user's usual key (often `F6`).
2. They type a throwaway replay bind. That is draft-only; the OS still owns the old key.
3. They focus Bookmark and press the old key.

Two things then happen at once:

- The still-registered Save Replay action fires.
- For function keys, `RegisterHotKey` swallows the press, so the Bookmark field may never see it.

That is a product bug, not user error. The same trap exists on first-run's replay field, and for
recording/bookmark binds that only go through the hook.

## Product decisions

- While a hotkey recorder field is focused (Settings or first-run), do not dispatch Save Replay,
  Start/Stop recording, or Bookmark.
- Unregister OS global shortcuts for that window too, so the key actually reaches the field.
- Keep pause active for the whole focus, including after a combo is captured and the status says
  "Ready to save." A second press in the same field is still a rebind, not a clip.
- Resume on blur, settings reload, first-run close, main-window destroy, and `frontend_ready`, so a
  tray close mid-capture cannot leave Save Replay dead.
- Do not live-apply unsaved drafts. Pause is capture-scoped so someone can keep Settings open on
  another monitor and still clip.

## Minimal architecture

A process-wide pause flag plus `effective_global_hotkeys(settings, paused)` so capture and
`save_settings` share one view of what the OS should own. The hook checks the flag before
dispatching. A `set_hotkey_capture_active` command flips the flag and syncs OS registrations.
The UI serializes those invokes so switching fields cannot resume-then-pause out of order.

## Plan-driven implementation

### Task 1: Native pause

- [ ] Hook test: a paused dispatcher matches no action, including the currently bound Save Replay
      key.
- [ ] App test: `effective_global_hotkeys` returns the configured F-key shortcuts when live and
      none while capturing, so `RegisterHotKey` cannot steal the key from the field.
- [ ] `set_actions_paused`, hook short-circuit, Tauri handler/`active_shortcut_matches` ignore,
      `save_settings` uses the effective set, resume on window destroy and `frontend_ready`.

### Task 2: UI

- [ ] UI contract: Settings and first-run capture call `set_hotkey_capture_active`; focus keeps
      pause until blur even after a successful capture.
- [ ] `beginHotkeyCapture` / blur / first-run focus-blur / first-run close.

### Task 3: Verify

- [ ] Workspace tests, warning-denied Clippy, handoff, relaunch for the swap repro.
