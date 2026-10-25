# Keep automatic recording armed when a game's recorder ends itself

Base: `develop` at `ee6a703a`. On 2026-10-25 Clipline recorded three League
games, then silently skipped the next four (00:26–01:35 local) until Dain
started a recording by hand. The log has nothing between 00:25 and 01:38.

Root cause: a detected game is captured by window handle. When League closes
its window, WGC ends the capture (`wgc.rs` returns `Ok(None)`), the recorder
finalizes the session and reports `recording: false` on its own. The game
detector polls every 500 ms; if it notices the window is gone first, it plans
a restart and the bumped generation makes that report stale. If the
recorder's report lands first, `accept_service_status` treats it like a user
stop and clears `recording_desired`. Every later detection then plans no
recorder, so automatic recording is off until the user turns it back on.

Nothing about this is logged: recorder `Error` and `StorageQuotaFull` events
only go to the webview (destroyed while trayed), and detection transitions
are not logged at all.

- [ ] Add a failing runtime test: a recorder serving a detected League
  window reports a matching `recording: false`; the game then disappears
  and a new League window appears. The recording intent survives and the
  new window plans a replacement recorder. The existing user-stop and
  stale-status tests keep passing.
- [ ] In `accept_service_status`, when the recorder that ended on its own
  was serving a detected game, clear the sender and any manual session but
  keep `recording_desired`. The detector owns the game lifecycle and the
  next transition decides what runs. A recorder that ends without a game
  (window-title or display capture) keeps today's behaviour.
- [ ] Log an `INFO` `recorder_ended_unrequested` event with whether a game
  was bound and whether the intent was kept.
- [ ] Log recorder `Error` events (`WARN recorder_error`) and accepted quota
  blocks (`WARN storage_quota_blocked` with total, quota and required bytes)
  from `pump_events`, so a trayed app leaves a trail.
- [ ] Log game detection transitions (`INFO game_detection_changed` with the
  game id or none and whether a recorder will start) from
  `plan_detection_transition` when the window changes.
- [ ] Run workspace tests and fresh-cache warning-denied Clippy, update the
  handoff, and create a PR into develop with green OS checks.

Plan checkboxes remain unticked by repository convention.
