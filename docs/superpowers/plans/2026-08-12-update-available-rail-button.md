# Update-available rail button

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** Surface a waiting update where the user will actually see it — a button in the left rail,
directly above Settings, that stays hidden until a newer build exists.

## Why

Updating is currently a pull, not a push: Settings → General → **Check for updates**. Clipline is a
background app people leave running for days, so a user who never opens Settings never learns a
Nightly shipped. The updater plumbing already exists (`check_for_updates` and `install_update`
commands, per-channel endpoints in `updates.rs`); what is missing is anything that checks on its
own and anywhere to show the answer.

## Design

**The poll lives in Rust, not the webview.** The recorder outlives any particular window, and the
window can be closed to tray while Clipline keeps recording. A `tauri::async_runtime` task started
in `run()`'s setup checks every 10 minutes and emits `update-available` when the answer is yes.

**10 minutes is cheap.** The endpoint is a GitHub *release asset*
(`releases/download/nightly/latest.json`) served from GitHub's CDN, so it is not subject to the
60/hour unauthenticated REST limit. 144 requests/day of a ~1 KB JSON costs nothing measurable.

**The poll stops once it finds an update.** The button does not need re-confirming, and a user who
declines the install should not be re-asked every ten minutes.

**Clicking confirms before installing.** `install_update` calls `Cmd::Stop` on the recorder and
restarts the app: a stray click mid-session would silently end that recording. The existing
`#update-dialog` is reused rather than duplicated — a second dialog would fight the first over
`pendingUpdate` and the shared `update-dialog-*` ids — and gains a line saying recording stops.

**The launch modal stays; the background one does not.** The existing 1.5s-after-launch check keeps
opening the dialog, as does Settings → Check for updates: the user is present for both. Only the
10-minute poll is silent, because it can land ten minutes into a game. Every path calls
`announceUpdate`, so the rail button outlives dismissing the dialog.

**A disabled channel is skipped, not failed.** `UpdateChannel::Stable` is not enabled yet
(`STABLE_CHANNEL_ENABLED = false`), so a user on that channel must not generate a warning every ten
minutes.

## Task 1: Emit an update-available signal from the app

- [ ] Failing test: the poll interval and the "stop after found" rule are expressed as testable
      constants/helpers rather than buried in the spawn closure.
- [ ] Extract the `UpdateCheckResult` construction out of the `check_for_updates` command so the
      poller and the command share one code path.
- [ ] Spawn the poll task in `run()` setup: first check shortly after startup, then every 10
      minutes; emit `update-available` with the check result and stop; skip disabled channels; log
      and keep going on network errors.

## Task 2: Show the button and confirm the install

- [ ] Failing test: `tests/ui_contract.rs` requires `id="rail-update"` immediately above
      `id="rail-settings"`, the update dialog ids, and the `update-available` listener wiring.
- [ ] Add the hidden rail button above Settings, plus an accent style so it reads as an action
      rather than another neutral rail icon.
- [ ] Add the confirm dialog: new version, a warning that recording stops and Clipline restarts,
      and Cancel / Update actions.
- [ ] Wire `main.js`: reveal on `update-available`, open the dialog on click, call `install_update`,
      surface failures through the existing error line.

## Task 3: Verify

- [ ] `cargo test --workspace` green, `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] Launch the app and confirm the button is absent on an up-to-date build.
- [ ] Confirm the dialog copy and that Cancel leaves the recorder untouched.
