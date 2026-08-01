# Slint migration parity ledger

Baseline commit: `5eea6c3`

This is the acceptance ledger for replacing the Tauri/WebView2 frontend. Every current behavior
has a stable token, a source/contract summary, a target owner milestone, and an acceptance check.
Status values: `not_started`, `in_progress`, `implemented`, `verified`, `waived`. A waiver must add
the approving product decision and replacement behavior; deleting a row is not a waiver.

`apps/clipline-app/tests/slint_migration_contract.rs` extracts the production
`tauri::generate_handler!` list and frontend event subscriptions. When a boundary is added, update
this ledger in the same change. Line numbers are deliberately omitted because symbols are more
stable; the baseline commit preserves exact historical locations.

## Commands

| Stable token | Current source and behavior | Target owner | Acceptance | Status |
|---|---|---|---|---|
| `command:save_replay` | `app.rs::save_replay`; enqueue a replay save without blocking UI. | M5 recorder controller | Automated save request/event test; manual tray and window save. | `not_started` |
| `command:restart_as_administrator` | `app.rs`; elevated relaunch, parent handoff, orderly quit. | M6 Windows shell | Automated argument/handoff tests; manual elevated-game flow. | `not_started` |
| `command:set_recording` | `app.rs`; start, stop, or arm games-only recording. | M5 recorder controller | Existing recorder tests plus Slint action-state test. | `not_started` |
| `command:get_settings` | `app.rs`; return the persisted settings snapshot. | M5 desktop controller | Snapshot parity fixture. | `not_started` |
| `command:minimize_main_window` | `app.rs`; settings-dependent tray/taskbar transition. | M6 lifecycle | Lifecycle state-machine test and manual Windows check. | `not_started` |
| `command:choose_media_folder` | `app.rs`; native picker and one-shot media authorization. | M8 settings | Picker cancel/select integration test. | `not_started` |
| `command:choose_replay_cache_folder` | `app.rs`; native replay-cache folder picker. | M8 settings | Picker cancel/select integration test. | `not_started` |
| `command:list_displays` | `app.rs`; enumerate displays and stable identifiers. | M8 capture settings | Device fixture and live multi-display check. | `not_started` |
| `command:list_audio_devices` | `app.rs`; enumerate output and microphone endpoints. | M8 capture settings | Device fixture and live add/remove check. | `not_started` |
| `command:probe_encoders` | `app.rs`; asynchronous hardware/FFmpeg encoder probe. | M8 recording settings | Existing probe tests and live Windows matrix. | `not_started` |
| `command:report_decode_support` | `app.rs`; frontend capability report influences Automatic encoder choice. | M8 native playback probe | Capability-to-encoder decision test. | `not_started` |
| `command:list_game_plugins` | `app.rs`; return installed game integration descriptors. | M8 games | Plugin presentation fixture. | `not_started` |
| `command:list_game_windows` | `app.rs`; enumerate candidate running windows. | M8 games | Window-enumeration adapter test. | `not_started` |
| `command:detect_installed_games` | `app.rs`; merge detected and custom games. | M8 games | Existing detector fixtures and stale-result test. | `not_started` |
| `command:extract_window_icon` | `app.rs`; return bounded icon data for a selected window. | M8 games | Size/error test and manual icon check. | `not_started` |
| `command:memory_status` | `app.rs`; cached in-app process-tree PWS diagnostic, not benchmark truth. | M5 diagnostics | Cross-check against external sampler; label scope. | `not_started` |
| `command:frontend_ready` | `app.rs`; startup warnings and authoritative lifecycle snapshot. | M5 bootstrap snapshot | Startup race and stale-revision tests. | `not_started` |
| `command:start_microphone_test` | `app.rs`; start native microphone monitor stream. | M8 capture settings | Start/idempotence/device-error tests. | `not_started` |
| `command:stop_microphone_test` | `app.rs`; synchronously stop microphone monitor. | M8 capture settings | Stop/background-entry tests. | `not_started` |
| `command:get_autostart_status` | `app.rs`; report configured per-user launch state. | M6 Windows shell | Registry adapter test and installed smoke test. | `not_started` |
| `command:check_for_updates` | `app.rs`; check selected signed update channel. | M6 updater | Manifest/offline/channel tests. | `not_started` |
| `command:install_update` | `app.rs`; recheck, stop services, download/install. | M6 updater | Signed/tampered/cancel/install tests. | `not_started` |
| `command:save_settings` | `app.rs`; transactional hotkey, tray, autostart, persistence, quota, and recorder update with compensation. | M8 settings controller | Failure-injection transaction suite. | `not_started` |
| `command:prepare_bug_report` | `app/support.rs`; sanitize and stage reviewable support bundle. | M8 support | Existing redaction and preparation tests. | `not_started` |
| `command:submit_bug_report` | `app/support.rs`; bounded cancellable upload of approved bundle. | M8 support | Upload/cancel/rate-limit fixture. | `not_started` |
| `command:cancel_bug_report` | `app/support.rs`; cancel active preparation/submission. | M8 support | Cancellation state-machine test. | `not_started` |
| `command:discard_bug_report` | `app/support.rs`; remove staged report. | M8 support | Cleanup/file-release test. | `not_started` |
| `command:save_prepared_bug_report` | `app/support.rs`; native save picker for reviewed bundle. | M8 support | Cancel/save picker test. | `not_started` |
| `command:open_diagnostics_folder` | `app/support.rs`; reveal diagnostics directory. | M6 Windows shell | Shell adapter test and manual Explorer check. | `not_started` |
| `command:diagnostics_location` | `app/support.rs`; return diagnostics path. | M8 support | Path/scope fixture. | `not_started` |
| `command:support_capabilities` | `app/support.rs`; report honest support workflow capabilities. | M8 support | Existing capability contract vectors. | `not_started` |
| `command:log_frontend_event` | `app/support.rs`; rate-limited and redacted UI diagnostics. | M5 diagnostics | Redaction/rate-limit test. | `not_started` |
| `command:cloud_status` | `cloud.rs`; registered legacy status endpoint, currently unused by JS. | M7 cloud controller | Explicit preserve/remove decision test. | `not_started` |
| `command:cloud_connect` | `cloud.rs`; validate endpoint/consent and store credentials. | M8 cloud settings | HTTPS/HTTP-consent/credential tests. | `not_started` |
| `command:cloud_disconnect` | `cloud.rs`; clear account credentials and state. | M8 cloud settings | Account-replacement and cache-state test. | `not_started` |
| `command:upload_clip_to_cloud` | `cloud.rs`; lease local clip and start upload. | M7 cloud controller | Lease/progress/cancel fixture. | `not_started` |
| `command:sync_cloud_clip_status` | `cloud.rs`; reconcile local and authoritative remote state. | M7 cloud controller | Windows path identity and stale-result tests. | `not_started` |
| `command:list_cloud_clips` | `cloud.rs`; paged authoritative remote listing. | M7 cloud controller | Pagination/generation fixture. | `not_started` |
| `command:cloud_clip_thumbnail` | `cloud.rs`; bounded cached remote thumbnail. | M7 cloud controller | Cache/quota/cancellation test. | `not_started` |
| `command:cache_cloud_clip_media` | `cloud.rs`; download and lease remote media. | M7 cloud controller | Cache limit, partial download, stale-open tests. | `not_started` |
| `command:cloud_user_profile` | `cloud.rs`; retrieve connected profile metadata. | M7 cloud controller | Account-generation fixture. | `not_started` |
| `command:cloud_user_avatar` | `cloud.rs`; retrieve bounded avatar bytes. | M7 cloud controller | Bounds/cache/error test. | `not_started` |
| `command:open_cloud_user_profile` | `cloud.rs`; open trusted profile URL. | M7 cloud controller | URL allowlist and shell test. | `not_started` |
| `command:open_cloud_clip` | `cloud.rs`; resolve/cache/open selected remote clip. | M7 cloud controller | Generation/cancel/media-lease test. | `not_started` |
| `command:osu_api_status` | `osu_api.rs`; report enrichment connection state. | M8 games | Credential/status fixture. | `not_started` |
| `command:save_osu_api_settings` | `osu_api.rs`; persist osu! API credentials/settings. | M8 games | Validation/credential rollback test. | `not_started` |
| `command:test_osu_api_connection` | `osu_api.rs`; cancellable live credential probe. | M8 games | Mock server success/failure test. | `not_started` |
| `command:open_osu_api_setup_guide` | `osu_api.rs`; open trusted setup documentation. | M8 games | URL allowlist and shell test. | `not_started` |
| `command:list_clips` | `library.rs`; scan local clips with metadata and markers. | M7 library controller | Existing scan fixtures and 2,000-clip run. | `not_started` |
| `command:clip_poster` | `library.rs`; bounded cached poster extraction. | M7 library controller | Semaphore/cache/stale-generation test. | `not_started` |
| `command:delete_clip` | `library.rs`; delete one clip and sidecars safely. | M7 library controller | Scope/file-lease/error test. | `not_started` |
| `command:delete_clips` | `library.rs`; partial-success bulk delete. | M7 library controller | Existing partial-success fixture. | `not_started` |
| `command:rename_clip` | `library.rs`; persist display-title metadata. | M7 library controller | Validation/refresh fixture. | `not_started` |
| `command:rename_clip_file` | `library.rs`; rename media and owned sidecars. | M7 library controller | Collision/scope/file-release test. | `not_started` |
| `command:export_clip` | `library.rs`; bounded keyframe-aligned trim/export. | M9 review controller | Existing byte/trim fixtures and live export. | `not_started` |
| `command:prepare_clip_audio_sidecars` | `library.rs`; current WebView multi-track playback sidecars. | M9 compatibility boundary | Preserve until native mix verified; file cleanup test. | `not_started` |
| `command:reveal_clip` | `library.rs`; select clip in Explorer. | M7 library controller | Shell scope test and manual Explorer check. | `not_started` |
| `command:copy_clip_to_clipboard` | `library.rs`; place original/shareable selection on clipboard. | M9 review controller | Original/trimmed modifier tests. | `not_started` |
| `command:open_media_folder` | `library.rs`; registered legacy folder action, currently unused by JS. | M7 library controller | Explicit preserve/remove decision test. | `not_started` |
| `command:storage_status` | `library.rs`; report media/cache quota and disk usage. | M8 storage settings | Quota/drive-error fixture. | `not_started` |

## Native-to-UI events

| Stable token | Current source and behavior | Target owner | Acceptance | Status |
|---|---|---|---|---|
| `event:status` | Recorder snapshot: active/waiting, buffer, encoder, backend, full-session. | M5 event sink | Snapshot/coalescing test. | `not_started` |
| `event:saved` | Save completion, path, duration, markers, GC/quota effects; triggers refresh and sound/enrichment side effects. | M5 event sink | Replay/session payload and side-effect tests. | `not_started` |
| `event:error` | User-visible runtime error string. | M5 event sink | Ordering and foreground-deferred error test. | `not_started` |
| `event:mic-test` | RMS, peak, count, and bounded PCM preview samples. | M8 microphone adapter | Stream bounds/level/playback test. | `not_started` |
| `event:mic-test-error` | Ends microphone test with visible failure. | M8 microphone adapter | Device-loss state transition. | `not_started` |
| `event:mic-test-stopped` | Reconciles microphone UI after native stop. | M8 microphone adapter | Explicit/background stop test. | `not_started` |
| `event:window-lifecycle` | Revisioned foreground/tray/taskbar/background snapshot. | M5 lifecycle snapshot | Stale-revision and bootstrap-race tests. | `not_started` |
| `event:game-detection` | Active game/window/process, mode, elevated-hotkey blockage. | M8 games | Detector stream and elevation warning test. | `not_started` |
| `event:cloud-upload-progress` | Path identity, byte progress, status, remote identity, error. | M7 cloud controller | Coalescing/account-generation test. | `not_started` |
| `event:osu-enrichment-updated` | Invalidates/refetches local Library after enrichment. | M7 library controller | Deferred foreground refresh test. | `not_started` |

## Surfaces and dialogs

| Stable token | Current contract | Owner | Acceptance | Status |
|---|---|---|---|---|
| `surface:persistent-shell` | Frameless titlebar, recorder controls/status, Save Replay, hotkey, game/memory/cloud summaries, Settings. | M6 shell + M7/M8 models | Snapshot, keyboard, DPI, Narrator checks. | `not_started` |
| `surface:local-library` | Search/filter/group/sort/page, poster states, selection, context actions. | M7 library | Model fixtures; 50/500/2,000-item manual matrix. | `not_started` |
| `surface:cloud-library` | Remote paging/cache/progress/account-aware actions. | M7 cloud | Cloud generation fixtures and manual flow. | `not_started` |
| `surface:review` | Playback, tracks, trim, markers, timeline, export/share/rename/delete. | M9 review | Cross-implementation vectors and media gates. | `not_started` |
| `surface:settings-general` | Startup, lifecycle, timeline, theme, update channel/check. | M8 settings | Draft/validation/accessibility tests. | `not_started` |
| `surface:settings-capture` | Displays/region, backend, audio devices/volumes, microphone test. | M8 settings | Device fixtures and multi-DPI manual flow. | `not_started` |
| `surface:settings-recording` | Basic/advanced encoder, duration, resolution, bitrate/quality, FPS. | M8 settings | Validation/normalization parity vectors. | `not_started` |
| `surface:settings-games` | Detection, games-only mode, plugins, custom/running game flows. | M8 settings | Plugin/detector fixtures and keyboard flow. | `not_started` |
| `surface:settings-storage` | Media/cache locations, quotas, wear acknowledgement. | M8 settings | Transaction/quota/picker tests. | `not_started` |
| `surface:settings-cloud` | Connect/disconnect, insecure HTTP consent, visibility, local-delete policy. | M8 settings | Consent/account replacement tests. | `not_started` |
| `surface:settings-hotkeys` | Two global shortcut recorders and validation feedback. | M8 settings + M6 shell | Parser/rollback/live hook tests. | `not_started` |
| `surface:settings-support` | Disclosure, prepare/preview/send/save/discard/cancel/report-ID workflow. | M8 support | Support state machine and accessibility test. | `not_started` |
| `dialog:delete-confirmation` | Single/bulk delete confirmation and counts. | M7 library | Focus trap, cancel, confirm tests. | `not_started` |
| `dialog:quit-confirmation` | Close without tray prompts before orderly quit. | M6 shell | Close-setting matrix. | `not_started` |
| `dialog:update-available` | Version/notes and install action. | M6 updater | Keyboard/accessibility and signed-install smoke. | `not_started` |
| `dialog:elevated-game-warning` | One-per-process warning and optional elevated restart. | M6 shell | Warning suppression/restart manual test. | `not_started` |
| `dialog:cloud-upload` | Title, audio, visibility, local-delete options. | M7 cloud | Validation/focus/upload fixture. | `not_started` |
| `dialog:detected-games` | Select detected games to configure. | M8 games | Keyboard/list update test. | `not_started` |
| `dialog:running-window` | Select a running window for a custom game. | M8 games | Refresh/cancel/select test. | `not_started` |
| `dialog:file-rename` | Validate and commit media filename rename. | M7 library | Enter/Escape/collision tests. | `not_started` |
| `dialog:game-plugin-settings` | Edit plugin-specific settings/modes. | M8 games | Schema/keyboard/validation tests. | `not_started` |
| `dialog:shortcut-guide` | Player shortcut reference and close controls. | M9 review | Mapping completeness/focus test. | `not_started` |
| `dialog:media-folder-picker` | Native media-folder selection. | M8 storage | Cancel/select/scope test. | `not_started` |
| `dialog:replay-cache-folder-picker` | Native replay-cache selection. | M8 storage | Cancel/select/scope test. | `not_started` |
| `dialog:support-bundle-picker` | Native save destination for prepared bundle. | M8 support | Cancel/save test. | `not_started` |

## Shortcuts and pointer gestures

| Stable token | Current contract | Owner | Acceptance | Status |
|---|---|---|---|---|
| `shortcut:global-save-replay` | Two configurable keyboard/mouse chords; default Alt+F10; Windows-reserved combinations rejected; low-level hook plus registered shortcut. | M6 hotkeys | Existing parser vectors, rollback tests, elevated live check. | `not_started` |
| `shortcut:play-pause` | Space/K toggles playback outside modal/settings contexts. | M9 review | Player intent vectors. | `not_started` |
| `shortcut:seek-five-seconds` | Left/Right seeks ±5 s. | M9 review | Player intent vectors. | `not_started` |
| `shortcut:seek-one-second` | Shift+Left/Right seeks ±1 s. | M9 review | Player intent vectors. | `not_started` |
| `shortcut:seek-source-frames` | J/L seeks ±10 source-frame intervals. | M9 review | Variable-FPS vectors. | `not_started` |
| `shortcut:seek-tenth-second` | Comma/period seeks ±0.1 s. | M9 review | Boundary clamp vectors. | `not_started` |
| `shortcut:set-trim-in-out` | I/O sets trim start/end. | M9 review | Trim invariant vectors. | `not_started` |
| `shortcut:marker-navigation` | M/Shift+M moves next/previous marker. | M9 review | Marker wrap/filter vectors. | `not_started` |
| `shortcut:edit-point-navigation` | Up/Down moves previous/next edit point. | M9 review | Combined rail vector. | `not_started` |
| `shortcut:timeline-zoom` | Plus/minus changes timeline zoom. | M9 review | Zoom anchor/bounds vectors. | `not_started` |
| `shortcut:timeline-fit` | Backslash fits trim; Shift+Backslash or Shift+Z fits clip. | M9 review | Fit-window vectors. | `not_started` |
| `shortcut:clip-boundary` | Home/End seeks start/end. | M9 review | Boundary vectors. | `not_started` |
| `shortcut:toggle-snapping` | S toggles timeline snapping. | M9 review | Snap state vector. | `not_started` |
| `shortcut:fullscreen` | F toggles fullscreen. | M9 review | Window transition test. | `not_started` |
| `shortcut:escape-context` | Escape closes Review/settings/dialog or clears Library selection according to focus context. | M7–M9 | Modal priority/focus tests. | `not_started` |
| `shortcut:shortcut-guide` | Shift+? opens the guide from the player. | M9 review | Dispatch/focus test. | `not_started` |
| `shortcut:library-select-all` | Ctrl+A selects visible page only while select mode is active. | M7 library | Paging/selection fixture. | `not_started` |
| `shortcut:settings-tab-navigation` | Left/Right/Home/End navigates Settings tabs. | M8 settings | Keyboard/ARIA parity test. | `not_started` |
| `gesture:timeline-seek` | Click timeline to seek. | M9 review | Pixel-to-time and settle tests. | `not_started` |
| `gesture:trim-edge-drag` | Drag either trim edge with ordering/minimum constraints. | M9 review | Pointer capture/trim invariant tests. | `not_started` |
| `gesture:trim-selection-drag` | Drag band to slide selection; unmoved click seeks. | M9 review | Clamp/click discrimination tests. | `not_started` |
| `gesture:snap-bypass` | Alt temporarily bypasses snapping. | M9 review | Modifier transition vectors. | `not_started` |
| `gesture:timeline-wheel-zoom` | Wheel on timeline/ruler zooms at pointer anchor. | M9 review | Wheel normalization/anchor tests. | `not_started` |
| `gesture:timeline-pan` | Shift-wheel or horizontal trackpad pans. | M9 review | Delta-mode and bound tests. | `not_started` |
| `gesture:overview-pan` | Overview drag pans visible range. | M9 review | Navigator geometry vectors. | `not_started` |
| `gesture:overview-edge-zoom` | Overview grips zoom; empty click recenters; wheel pans. | M9 review | Pointer-mode geometry vectors. | `not_started` |
| `gesture:marker-seek` | Marker chip/tick click seeks and selects. | M9 review | Marker filter/seek vectors. | `not_started` |
| `gesture:play-block-seek` | osu! play block click seeks/holds selection. | M9 review | Play-rail vectors. | `not_started` |
| `gesture:shift-fit-whole-clip` | Shift-click zoom-fit selects whole-clip fit. | M9 review | Modifier behavior test. | `not_started` |
| `gesture:shift-copy-original` | Shift-click clipboard action copies original instead of selection. | M9 review | Clipboard argument test. | `not_started` |
| `gesture:context-menu` | Right-click clips/displays opens custom context menu with keyboard dismissal. | M7/M8 | Menu focus/position/action tests. | `not_started` |
| `gesture:stage-overlay-activity` | Pointer activity reveals transport overlay then fades it. | M9 review | Timer/focus/fullscreen test. | `not_started` |

## Tray, lifecycle, updater, and packaging

| Stable token | Current contract | Owner | Acceptance | Status |
|---|---|---|---|---|
| `tray:open` | Open/rebuild/reveal the main window. | M6 shell | Tray-only and destroyed-window smoke tests. | `not_started` |
| `tray:save-replay` | Save Replay with dynamically configured hotkey label. | M6 shell | Label/action/recording tests. | `not_started` |
| `tray:open-diagnostics` | Open diagnostics directory. | M6 shell | Shell error/manual Explorer test. | `not_started` |
| `tray:quit` | Stop mic/recorder and allow explicit exit. | M6 shell | Orderly shutdown/file-finalization test. | `not_started` |
| `tray:left-click-open` | Left-button release opens; other transitions do not. | M6 shell | Existing mouse-event matrix. | `not_started` |
| `lifecycle:normal-launch` | Start services, build tray, reveal initially hidden window. | M6 shell | Cold-start readiness measurement. | `not_started` |
| `lifecycle:autostart-tray` | `--autostart` starts services without revealing UI. | M6 shell | Installed autostart and zero-window gate. | `not_started` |
| `lifecycle:single-instance-reveal` | Secondary non-autostart launch reveals primary. | M6 shell | Same-user activation tests. | `not_started` |
| `lifecycle:close-to-tray-or-quit` | Close obeys setting; background stops mic and releases review work. | M6 shell | Setting matrix and 100-cycle soak. | `not_started` |
| `lifecycle:minimize-to-tray-or-taskbar` | Minimize obeys setting and publishes revisioned mode. | M6 shell | Setting matrix and focus restore test. | `not_started` |
| `lifecycle:foreground-bootstrap-snapshot` | Ready call returns authoritative revision so stale async work cannot win. | M5 controller | Startup race/generation suite. | `not_started` |
| `updater:silent-check` | Delayed startup check on selected channel without interrupting on no update. | M6 updater | Timer/channel/offline test. | `not_started` |
| `updater:manual-check` | User check shows current/update/error result. | M6 updater | State and keyboard test. | `not_started` |
| `updater:install` | Recheck, stop services, verify/download, passive install, exit. | M6 updater | Signed/tampered/cancel/install matrix. | `not_started` |
| `updater:signature-verification` | Existing committed minisign public key gates artifacts. | M6 updater | Known-good/bad signature tests. | `not_started` |
| `package:regular-nsis` | Per-user NSIS with Evergreen WebView bootstrap during coexistence. | M6/M10 packaging | Clean install/upgrade/uninstall VM smoke. | `not_started` |
| `package:standalone-nsis` | Separate standalone manifest/runtime/update channel. | M6/M10 packaging | Cross-update refusal and upgrade smoke. | `not_started` |
| `package:webview2-runtime` | Required/bundled WebView runtime remains until cutover, then is removed with Tauri. | M10 cutover | Bundle inspection and repair-path decision. | `not_started` |
| `package:ffmpeg-resource` | Reviewed LGPL FFmpeg remains a separately spawned resource. | M6/M10 packaging | Existing integrity/license verifier. | `not_started` |
| `package:product-identity` | Preserve Clipline, `io.clipline.app`, per-user settings/credentials/media/shortcuts. | M10 cutover | Installed migration and rollback test. | `not_started` |

## CLI and cross-cutting manual matrix

The shell replacement must also preserve `--autostart`, `--window <title>`, `--lol-url <url>`,
`--disk-quota-gb <value>`, and the elevated-parent handoff. These are acceptance inputs for M6/M8,
not frontend callbacks. Every surface is checked at 100%, 125%, 150%, and 200% display scaling,
with keyboard-only operation and Narrator/UI Automation checks for custom controls. The existing
225 frontend tests remain the behavioral oracle until equivalent Rust/Slint tests pass.
