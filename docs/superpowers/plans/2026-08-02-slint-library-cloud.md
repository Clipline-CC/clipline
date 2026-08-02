# Slint Milestone 7: Native Library and Cloud

> **For agentic workers:** Execute one task at a time with failing tests first. Commit this plan
> before implementation. Keep checkboxes unticked by repository convention.

**Program milestone:** Milestone 7 of `2026-08-01-slint-frontend-replacement.md`.

**Goal:** Replace Clipline's WebView Library and Cloud presentation with a bounded, framework-
neutral Rust catalog and reusable local/cloud services, then expose the complete Local and Cloud
gallery experience through the Slint candidate. Preserve the shipping Tauri behavior while it is
the production adapter and prove that 2,000 clips do not become 2,000 Slint rows or decoded poster
images.

**Baseline:** branch `agent/slint-frontend-replacement-plan` after Milestone 6 closeout `7a34962`.
The user's installed Clipline remains open. Never stop it, mutate its settings/credentials/media,
or install an internal candidate. The branch may remain local while GitHub rejects workflow-file
pushes from the current OAuth token, which lacks the `workflow` scope.

**Non-goals:** Porting the Settings/Cloud-account editor, Games or microphone surfaces (Milestone
8); porting the complete Review/editor, export, audio-sidecar, or clipboard-share UI (Milestone 9);
cutting over the production binary, deleting JavaScript/Tauri/WebView2, changing settings JSON,
credential targets, cloud API protocol, media output bytes, installer identity, or updater behavior
(Milestone 10); linking FFmpeg; claiming manual accessibility or performance evidence not run.

## Architecture and hard contracts

Add a framework-neutral `clipline-library` workspace crate. It owns local/cloud catalog DTOs,
Windows-equivalent path identity, checked request/account generations, gallery paging/filtering/
sorting/grouping, controller-owned selection and active-clip identity, presentation rows, service
ports, and bounded result delivery. It must not depend on Tauri, Slint, WebView2, DOM/JavaScript,
or application binaries. `clipline-desktop` remains the durable cross-surface shell snapshot; it
continues to carry the Library revision and bounded upload summaries, not thousands of catalog
rows.

Move the existing local scan, rename/delete safety transactions, poster coordinator, cloud list/
asset/cache/profile/status logic, and durable upload orchestration behind reusable services. The
shipping Tauri commands become JSON-compatible adapters over those services. The Slint candidate
uses the same services directly. Do not make the Slint package depend on `clipline-app` and do not
duplicate filesystem, cloud, or upload algorithms in a spike-only module.

The full `AppSettings` document type, normalization, load/repair/backup, and atomic persistence must
move intact into a framework-neutral `clipline-settings` crate before live Slint services are
wired. This is a storage-boundary prerequisite, not the Milestone 8 Settings UI port. Inject narrow
`LibraryConfig`, `CloudAccountStore`, and upload-record transaction ports backed by that shared
whole-document store. Neither Library nor Slint may partially deserialize or rewrite
`settings.json`. Cloud credential access stays behind the reviewed `clipline-shell` safe wrapper.

The catalog controller owns a compact bounded metadata index, current source, query/filter/
sort/group, page, selected path identities, active clip identity, modal/context-menu target, and
checked generations. Slint receives only the active page (at most 60 rows), active dialog state,
and bounded upload summaries. Row instances are projections and never own selection or active
identity. The compact index stores marker counts/search/presentation summaries, not every clip's
full marker/play/audio vectors; bounded detail is resolved only for the active page or Review.
Destroying/recreating the window rebuilds the same projection from controller state.

All filesystem, FFmpeg, image decode, HTTP, cache, and credential work stays off the Slint event
loop. Results carry exact `{attachment, foreground, request}` ownership and, for cloud work, exact
`{account_key, account_generation}` ownership. Stale window-scoped work is released without state
mutation. Durable uploads may outlive a window but may never persist or publish into a replacement
cloud account.

Pinned bounds and behavior:

- gallery page: 60 rows; poster result LRU: 120 path/status entries including negative results;
  decoded image window: at most 32 visible/near-visible images retained by the presentation adapter;
- local index: at most 10,000 metadata rows per scan, with an explicit truncated/error state rather
  than unbounded collection; root scan failure is fatal while unreadable session folders remain
  visible warnings; marker/metadata/session sidecars have explicit byte, entry-count, and string
  bounds, with corrupt/oversized sidecars becoming per-clip/session warnings;
- poster extraction: canonical-path single flight, at most two FFmpeg children, 480-pixel output,
  4 MiB output, 64 KiB stderr, 30-second deadline, atomic owned-temp publication;
- cloud list: server-side 60-row pages for the Slint adapter, with conservative next-page state
  because the pinned API exposes neither total nor `has_more`; the current API's 100-page /
  10,000-item compatibility ceiling retained only inside the Tauri compatibility adapter where
  exact old JSON requires a complete list;
- cloud thumbnail: 10 MiB; avatar: 2 MiB; media: 4 GiB hard cap; cache: 10 GiB, 2 GiB free-space
  floor, seven-day age; temp ownership: 24 hours; active playback lease: scoped ownership rather
  than a speculative 24-hour lease;
- upload: two concurrent uploads, 64 MiB maximum part, three bounded retries, streamed slices,
  active-file lease blocking rename/delete, and at most 16 upload summaries in the desktop snapshot;
- UI result inboxes are bounded and nonblocking. Page/poster/progress replacement coalesces only
  within the exact ownership token. Mutations, terminal upload results, and foreground feedback are
  durable or fail with a typed capacity error; no silent drops.

Stop the milestone and record a no-go before advancing if the Slint package depends on Tauri or
`clipline-app`, if filesystem/network/poster work runs on the UI thread, if more than 60 catalog
rows or more than 32 decoded/retained page images enter the Slint model, if stale/account-crossed work mutates or persists state,
if an active upload source can be renamed/deleted, if abandoned media-open work retains a playback
lease, or if the 2,000-clip absolute memory/process bound fails.

## Task 1: Freeze the neutral catalog and asynchronous ownership contract

**Files**

- Modify: `Cargo.toml`
- Create: `crates/clipline-library/Cargo.toml`
- Create: `crates/clipline-library/src/lib.rs`
- Create: `crates/clipline-library/src/identity.rs`
- Create: `crates/clipline-library/src/contract.rs`
- Create: `crates/clipline-library/src/channel.rs`
- Create: `crates/clipline-library/tests/contract.rs`
- Create: `crates/clipline-library/tests/channel.rs`
- Modify: `apps/clipline-app/tests/repository_security.rs`

**Test first**

- Port every Windows path-identity vector from `gallery_window_core.rs` and
  `PlayerCore.sameClipPath`: drive, UNC, `\\?\`, `\\?\UNC\`, slash, case, whitespace, empty,
  relative, and non-Windows exact paths. Empty paths never become equal merely because both keys
  are empty.
- Define owned, serializable local/cloud item, account, upload, action, result, mutation-report,
  warning, page, and presentation DTOs. Pin field names used by current Tauri JSON fixtures.
- Add checked `CatalogRevision`, `RequestGeneration`, `ForegroundGeneration`, and
  `CloudAccountGeneration`; overflow is typed and never wraps/saturates.
- Pin exact work tokens. Window work requires attachment + foreground + request generations;
  cloud work additionally requires account generation and stable account key. A durable upload
  token omits attachment but includes source path identity and exact account ownership.
- Add a fixed-capacity result port. Coalescable page/poster/byte-progress results replace only an
  older result for the same token/key and cannot cross a durable mutation/terminal barrier. Full,
  stale, account-changed, generation-exhausted, and disconnected outcomes are distinct.
- Add a failing same-local-clip-ID/different-account test to the existing desktop channel and
  controller before surface work. Cloud progress may coalesce only inside the same opaque account
  generation and upload generation; credential targets/secrets never become coalescing keys.
- Reject Tauri/Slint/Win32 imports in neutral modules, `unsafe` outside reviewed Windows modules,
  arbitrary maps in result payloads, and direct dependency from the spike to `clipline-app`.

**Implement to green**

- Add `clipline-library` as a normal workspace member with only neutral dependencies initially.
- Keep paths as owned original strings/`PathBuf`s plus explicit `ClipPathIdentity`; never lowercase
  the path used for I/O.
- Make command/result payload size bounds and collection maxima public constants with compile-time
  assertions where possible.

## Task 2: Port gallery and CloudCore behavior to Rust with cross-implementation fixtures

**Files**

- Create: `crates/clipline-library/src/gallery.rs`
- Create: `crates/clipline-library/src/cloud_model.rs`
- Create: `crates/clipline-library/tests/gallery.rs`
- Create: `crates/clipline-library/tests/cloud_model.rs`
- Create: `crates/clipline-library/tests/js_parity.rs`
- Modify: `apps/clipline-app/tests/gallery_window_core.rs`
- Modify: `apps/clipline-app/tests/cloud_core.rs`
- Modify: `apps/clipline-app/tests/player_core.rs`

**Test first**

- Port `GalleryWindowCore`: default/validated page size, page count, clamping, identity reset,
  ungrouped windows, split group boundaries with full group counts, LRU get/set/eviction, and exact
  missing-FFmpeg classification.
- Pin the 50/500/2,000 cases: one active window contains respectively 50/60/60 rows and never more
  than 32 decoded visible/overscan images; empty/out-of-range pages remain safe.
- Port current local filter/sort/group/search rules: all/replay/session/trim/marked; newest/oldest/
  largest/marks; smart/day/game/session/none; display-title/name/session/game search; stable
  newest-first ordering inside groups with `ClipPathIdentity` as the deterministic final tie-break.
- Port the bounded card-presentation subset currently called through `PlayerCore`: clip kind,
  session grouping, marker digest/category colors, gallery card preview, custom title policy, and
  first-party/plugin game summary/icon identity. Keep the rest of PlayerCore for Milestone 9.
- Define a bounded `ClipDetail` request/result for one stable item: marker ticks/digest, audio-track
  IDs/labels, and upload-dialog summary strings. Enforce sidecar byte/entry/string/audio-track bounds
  and exact item + foreground + request ownership so a late detail cannot populate a replacement
  row/dialog.
- Port `CloudCore`: account key, request gate, backend-owned settings merge, exact plain-HTTP
  consent, uploaded/shareability rules, and progress reconciliation that does not rebuild cards for
  byte-only bursts.
- Port the authoritative-server/local-upload merge vectors from `PlayerCore.cloudLibraryEntries`,
  including Windows-equivalent paths, private clips, processing clips, missing remotes, and
  duplicate identity.
- Run one canonical JSON corpus through JavaScript and Rust and compare byte-equivalent normalized
  outputs. Keep the JavaScript tests until Milestone 10; Rust becomes the new source of truth only
  after the cross-runner is green.

**Implement to green**

- Use deterministic ordering with explicit tie-breakers; do not rely on hash-map iteration.
- Keep the full metadata index free of Slint image objects or framework handles.
- Preserve the current page-reset semantics when source/filter/group/page-size/data identity changes;
  otherwise clamp the current page.

## Task 3: Extract the shared whole-document settings store

**Files**

- Modify: `Cargo.toml`
- Create: `crates/clipline-settings/Cargo.toml`
- Create: `crates/clipline-settings/src/lib.rs`
- Create: `crates/clipline-settings/src/types.rs`
- Create: `crates/clipline-settings/src/cloud.rs`
- Create: `crates/clipline-settings/src/games.rs`
- Create: `crates/clipline-settings/src/osu.rs`
- Create: `crates/clipline-settings/src/validation.rs`
- Create: `crates/clipline-settings/src/persistence.rs`
- Create: `crates/clipline-settings/tests/persistence.rs`
- Modify: `apps/clipline-app/src/settings/mod.rs`
- Modify: `apps/clipline-app/src/settings/*.rs`
- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-slint-spike/Cargo.toml`
- Create: `apps/clipline-slint-spike/src/settings.rs`
- Create: `apps/clipline-slint-spike/tests/settings_store.rs`

**Test first**

- Move the full current settings load/save/repair/normalization/default/backup/atomic-temp corpus
  unchanged into the shared crate. Preserve every JSON field/default, unknown-field policy, path,
  last-known-good backup, corrupt-file quarantine, normalization, and error string used by callers.
- Prove whole-document compare-and-swap transactions validate the expected settings revision and
  account generation before applying a narrow media-root/cloud-record/profile change. A failed or
  stale transaction preserves primary, backup, in-memory value, and revision byte-for-byte.
- Keep secrets absent from settings JSON and use the exact existing Credential Manager target.
- The Tauri settings module becomes a compatibility re-export/adapter over the shared type/store;
  all existing settings and transaction tests remain green without changing command JSON.
- Add a Slint candidate bootstrap that opens only an explicitly supplied isolated profile for tests
  or the normal shared Clipline profile for the eventual production candidate, obtains a complete
  typed document, and persists only through the same transaction API. Tests may never resolve to
  or mutate the installed user's profile.

**Implement to green**

- Move the storage/type implementation rather than copying it. UI draft/dirty/save compensation,
  device probes, and settings controls remain Milestone 8 work.
- Keep framework/Win32 imports out of `clipline-settings`; inject config-directory/path resolution
  where platform ownership requires it.
- Use this concrete store for `LibraryConfig` and `CloudAccountStore` in both adapters. Milestone 7
  cannot claim live Slint Cloud/upload parity until this task is complete.

## Task 4: Extract the local Library service without changing Tauri JSON

**Files**

- Create: `crates/clipline-library/src/local.rs`
- Create: `crates/clipline-library/src/naming.rs`
- Create: `crates/clipline-library/src/repository.rs`
- Create: `crates/clipline-library/tests/local_scan.rs`
- Create: `crates/clipline-library/tests/local_mutation.rs`
- Create: `crates/clipline-library/tests/clip_detail.rs`
- Modify: `apps/clipline-app/src/library.rs`
- Modify: `apps/clipline-app/src/library/naming.rs`
- Modify: existing Library unit/command/JSON tests

**Test first**

- Move the existing scan vectors: root + one session level, MP4-only, newest-first, marker-derived
  duration, inferred legacy audio tracks, metadata title/kind, session/marker game identity, fatal
  root error, and per-session partial warnings.
- Add bounded sidecar parsing tests for file bytes, marker/play/audio entry counts, nesting/strings,
  corrupt JSON, hostile duration values, and aspect/search summaries. The compact scan result keeps
  only card/search summaries; active-page detail loading is separately bounded and token-fenced.
- Preserve canonical media-root validation: only root or one direct session child, `.mp4`, no
  traversal/symlink/reparse escape. Retain original spelling only for display and path-identity
  reconciliation; every read/open/mutation uses the separately stored canonical, containment-
  validated path and revalidates identity immediately before rename/delete to close TOCTOU swaps.
- Preserve title rename, file rename, delete, and bulk partial-success semantics. File rename moves
  MP4/markers/metadata/pending-osu/poster transactionally, rejects collisions and active-upload
  leases, and rolls every completed step back on failure. Delete owns exactly the existing sidecars
  and rejects an active upload source.
- Model reveal/folder-open as typed effects with validated paths. Platform execution stays in the
  existing safe application/Windows helper.
- Pin old Tauri command names, argument casing, result JSON, error strings used by contracts, and
  asset-protocol scoping. The compatibility adapter adds scope after a successful scan; the shared
  service itself has no Tauri concept.
- Accept a 10,000-item safety ceiling for the shipping scan: retain the exact `LocalClipScan` JSON
  shape, return the deterministic newest 10,000 items, and add one explicit warning string when
  truncated. Tests pin selection order and warning text; no silent omission is allowed.

**Implement to green**

- Move, do not fork, naming/scan/mutation algorithms into `clipline-library`; re-export DTOs where
  needed to avoid an all-at-once call-site rewrite.
- Provide synchronous repository operations suitable for `spawn_blocking`; adapters own executor
  choice and cancellation.
- Defer storage settings/status to Milestone 8 and export/audio sidecars/clipboard sharing to
  Milestone 9, while retaining their existing shipping implementations over shared path validation.

## Task 5: Extract the bounded poster service and native image ownership model

**Files**

- Create: `crates/clipline-library/src/poster.rs`
- Create: `crates/clipline-library/tests/poster.rs`
- Create: `crates/clipline-library/tests/poster_controller.rs`
- Modify: `crates/clipline-library/Cargo.toml`
- Modify: `apps/clipline-app/src/library.rs`
- Modify: `apps/clipline-app/src/poster.rs`
- Modify: `apps/clipline-slint-spike/Cargo.toml`
- Create: `apps/clipline-slint-spike/src/poster.rs`
- Create: `apps/clipline-slint-spike/tests/poster_adapter.rs`
- Modify: `apps/clipline-app/tests/repository_security.rs`

**Test first**

- Preserve canonical-path single flight and prove at most two extractor children under concurrent
  requests. A waiter never starts a duplicate process and receives the same owned result.
- Preserve the 480-pixel, 4 MiB output, 64 KiB stderr, 30-second deadline, bounded pipe draining,
  owned temp, atomic publish, cache-hit, corrupt output, timeout, and child cleanup vectors.
- Add neutral viewport ownership: a page contains at most 60 rows but at most 32 visible/overscan
  posters may be queued/decoded/retained; leaving/replacing a page
  cancels queued work, ignores in-flight stale results, and releases all ready image handles outside
  the new window. LRU state stays at 120 including negative entries.
- Token-fence every result by path + attachment + foreground + page/poster generation. A renamed,
  deleted, filtered, cloud-switched, hidden, or destroyed row cannot receive an old poster.
- A background adapter opens the returned native file path, validates encoded bytes/dimensions and
  a decoded-pixel cap, decodes JPEG into a sendable `SharedPixelBuffer<Rgb8Pixel>`, closes the file,
  then constructs `slint::Image::from_rgb8` on the UI thread after the final token check. Structural
  tests reject base64, data URLs, unbounded file/pixel buffers, UI-thread file decode, and more than
  32 retained `slint::Image`s. Delete/rename-after-decode proves no file handle survives.
- Use `image` 0.25 with `default-features = false` and only `jpeg`, after repository license/security
  review. Read dimensions first, reject width/height/pixel-count/RGB-byte overflow against the
  public cap before allocation, then decode exactly RGB8 into a prevalidated bounded buffer.

**Implement to green**

- Keep FFmpeg a separate LGPL process. Reuse the staged/runtime locator and first-party process
  lifecycle helpers; never add linked media libraries.
- Keep extraction/cache policy neutral and file-backed ownership explicit. `slint::Image` is not
  `Send` in 1.17.1 and `Image::load_from_path` uses a thread-local cache, so do not perform file
  decode in callbacks: decode to the sendable pixel buffer on a worker, then construct/publish the
  image through the event loop with a weak component and current token.

## Task 6: Extract cloud account, list, thumbnail, profile, and cache services

**Files**

- Create: `crates/clipline-library/src/cloud.rs`
- Create: `crates/clipline-library/src/cloud/cache.rs`
- Create: `crates/clipline-library/src/cloud/cache_identity.rs`
- Create: `crates/clipline-library/src/cloud/ports.rs`
- Create: `crates/clipline-library/tests/cloud_service.rs`
- Create: `crates/clipline-library/tests/cloud_cache.rs`
- Modify: `crates/clipline-library/Cargo.toml`
- Modify: `apps/clipline-app/src/cloud.rs`
- Modify: `apps/clipline-app/src/cloud/cache_identity.rs`
- Modify: `apps/clipline-app/Cargo.toml`
- Modify: `apps/clipline-slint-spike/Cargo.toml`

**Test first**

- Inject exact account snapshots and transaction ports; every request/result includes the stable
  account key and checked account generation. Disconnect/reconnect, host/user/credential changes,
  and explicit invalidation make prior results stale without affecting replacement work.
- Add true server-paged list requests with page size 60, query/filter/sort inputs, bounded response
  parsing, cancellation, and no all-pages collection in the Slint path. Because the pinned response
  has no total/`has_more`, a short page is terminal and a full page exposes a conservative Next;
  an empty following page steps back without claiming an exact total/page count. Retain the current
  100 x 100 / 10,000 compatibility collection only for old Tauri `list_cloud_clips` JSON.
- Preserve status, user profile/avatar, open-profile/browser-page open, thumbnail and media cache
  behavior; explicitly pin that `open_cloud_clip` opens the owner's public browser page, while
  in-app remote playback is cache-media followed by a validated local open;
  pin public/private/processing shareability and exact remote/local identity rules.
- Preserve thumbnail 10 MiB, avatar 2 MiB, media 4 GiB, cache 10 GiB, free-space floor, age, temp,
  LRU-pair and protected-entry rules. Add backend asset-key single flight and a hard concurrency
  bound so UI scheduling is not the safety boundary.
- Replace speculative media-open leasing with a scoped `CloudMediaLease`. A canceled/stale download
  drops its temp and holds no playback lease; a current accepted open acquires the lease before the
  player receives the path and releases it when the active clip closes/replaces.
- Tauri commands remain thin adapters with exact names/JSON. Tauri asset scope is added only in the
  adapter after a validated cached asset succeeds.

**Implement to green**

- Move cloud transport/cache logic into the shared crate and pin the existing cloud API revision;
  do not duplicate requests in the Slint package.
- Add only the already reviewed `clipline-cloud-api`, `reqwest` rustls, `tokio`, hashing/time, and
  `clipline-settings` dependencies/features required by this service; record license/security
  review and reject native-TLS or framework dependency creep.
- Keep connect/disconnect/settings controls in Milestone 8, but accept account-snapshot changes now
  and invalidate every window-scoped request atomically.
- Browser/profile/clip opening remains a typed platform effect executed by reviewed helpers.

## Task 7: Make upload, status sync, persistence, and foreground feedback account-safe

**Files**

- Create: `crates/clipline-library/src/upload.rs`
- Create: `crates/clipline-library/tests/upload.rs`
- Create: `crates/clipline-library/tests/upload_account_fence.rs`
- Modify: `crates/clipline-library/Cargo.toml`
- Modify: `apps/clipline-app/src/cloud_upload.rs`
- Modify: `apps/clipline-app/src/cloud.rs`
- Modify: `crates/clipline-desktop/src/event.rs`
- Modify: `crates/clipline-desktop/src/snapshot.rs`
- Modify: `crates/clipline-desktop/src/channel.rs`
- Modify: `crates/clipline-desktop/src/controller.rs`
- Modify: corresponding desktop/app upload tests

**Test first**

- Port upload payload, two-worker, 64 MiB part, three-retry, cancellation, content-type, audio
  selection, terminal status, delete-after-success, and source-lease tests without changing bytes.
- Reproduce disconnect/reconnect during upload. A completion from account A cannot persist into,
  mutate, coalesce with, or notify account B. The source lease still releases and owned temps clean
  up. Same-account window destruction does not cancel a durable upload.
- Reject or join a concurrent upload of the same `(account_generation, local_clip_id)` before work
  starts. Record commits compare the exact upload generation inside the transaction, so an older
  completion can never overwrite a newer queued/uploading/terminal record even if tasks finish out
  of order.
- Status-sync persistence uses compare-and-swap on account generation, local identity, and the
  expected prior record/revision. A delayed sync cannot replace a newer upload record. Profile
  identity refresh applies the same account-generation check before updating Cloud identity.
  Delayed mock responses crossing account/record replacement must leave current state byte-identical.
- Extend progress ownership with account generation/key while retaining per-local-clip coalescing
  inside one account and the 16-summary bound. Byte-only progress does not rebuild catalog rows;
  remote identity/error/terminal transitions do.
- Preserve path reconciliation across case/slash/device-prefix-equivalent Windows paths after file
  rename, delete, status sync, and upload completion.
- Foreground feedback is stored as bounded durable state when no window is attached, presented once
  to the next current foreground, and acknowledged by exact notice ID. Stale-window completion may
  update durable upload state but may not touch a dead component.
- Active-file lease lookup uses `ClipPathIdentity`; local rename/delete and delete-local-after-
  upload observe the same lease owner atomically.
- Acquire the original validated clip's lease before selected-audio remix/remux preparation and
  hold it through transport, persistence, delete-after-success, and cleanup. The owned temporary
  payload has separate create-new/Drop ownership; leasing only that temp must fail a regression
  test because it permits an original-path rename/delete race.

**Implement to green**

- Inject a whole-settings upload-record transaction rather than letting the Library crate write
  settings JSON. Validate account ownership again inside the transaction immediately before commit.
- Route progress through the existing bounded desktop event port after extending its account fence;
  do not add an unbounded upload channel.

## Task 8: Build the bounded catalog controller and rebuildable projection

**Files**

- Create: `crates/clipline-library/src/controller.rs`
- Create: `crates/clipline-library/src/presentation.rs`
- Create: `crates/clipline-library/tests/controller.rs`
- Create: `crates/clipline-library/tests/presentation.rs`
- Modify: `crates/clipline-desktop/src/action.rs`
- Modify: `crates/clipline-desktop/src/snapshot.rs`
- Modify: `apps/clipline-slint-spike/src/desktop.rs`
- Modify: `apps/clipline-slint-spike/tests/desktop_adapter.rs`

**Test first**

- Pin Local/Cloud source, search/filter/sort/group/page, previous/next, enter/exit selection mode,
  toggle, page-only Select All, context target, active clip, rename/delete/upload dialog, refresh,
  reveal/open/share, and modal confirm/cancel actions as typed controller inputs/effects.
- Keep complete metadata bounded by the catalog cap but publish at most 60 presentation rows. Rows
  carry stable identity, visible strings/badges, selection/active booleans, and poster state; they do
  not own selected/active state or service handles.
- Selection survives page/filter/render changes, prunes only when the authoritative local scan
  removes paths, and uses Windows-equivalent identity. Cloud source exits local multi-select.
  Select All touches only current visible local rows. Escape closes modal/menu, clears selection,
  exits select mode, then returns from Review in the current documented priority order.
- Refresh/results are transactional: validate token/account and the entire replacement before
  swapping. Stale, truncated-policy, invalid row, and allocation errors preserve the prior state
  byte-for-byte.
- A Library revision or enrichment event requests a current-generation refresh; bursts coalesce.
  Destroy/recreate preserves durable controller state and produces the same active page without
  replaying events. Hiding drops page image handles while retaining metadata/selection/active ID.
- Add deterministic projection snapshots for empty/loading/error, 50/500/2,000 local, disconnected
  Cloud, Cloud loading/error/page, uploads, selection, pagination, menus, and dialogs.

**Implement to green**

- Keep service execution outside the reducer/controller. The controller emits bounded effects and
  accepts typed completions.
- Add only revision/source/active summaries to `DesktopSnapshot`; keep the catalog controller as a
  long-lived shell-owned service so the snapshot never balloons with item arrays.

## Task 9: Implement the Slint Local/Cloud surface and keyboard/accessibility contract

**Files**

- Modify: `apps/clipline-slint-spike/ui/app.slint`
- Create: `apps/clipline-slint-spike/ui/library.slint`
- Create: `apps/clipline-slint-spike/ui/dialogs.slint`
- Modify: `apps/clipline-slint-spike/src/lib.rs`
- Modify: `apps/clipline-slint-spike/src/model.rs`
- Modify: `apps/clipline-slint-spike/src/shell.rs`
- Modify: `apps/clipline-slint-spike/src/live.rs`
- Modify: `apps/clipline-slint-spike/src/controller.rs`
- Create: `apps/clipline-slint-spike/src/catalog.rs`
- Create: `apps/clipline-slint-spike/tests/library_surface.rs`
- Create: `apps/clipline-slint-spike/tests/library_keyboard.rs`
- Modify: `apps/clipline-slint-spike/tests/controller.rs`
- Modify: `apps/clipline-slint-spike/tests/windows_live.rs`
- Modify: `apps/clipline-slint-spike/tests/spike_contract.rs`

**Test first**

- Compile-contract every property/callback and exact model bound. No representative fixture rows
  remain in production construction; empty/loading/error states come from the controller.
- Reproduce Local/Cloud tabs, query/filter/sort/group controls, pagination, count/range label,
  bounded responsive cards, kind/duration/marker/game/upload badges, active/selected styling,
  select mode/bulk bar, context menus, title/file rename, delete confirmation/partial report, upload
  dialog/progress, copy/open public link, reveal, and cloud-media open.
- Match current surface scope exactly: Cloud has search/paging/context actions but hides local
  filter/sort/group controls and exits local multi-select. The upload dialog owns title (140 UTF-16
  code units, matching the browser `maxlength`), description (5,000 UTF-16 code units), visibility,
  and audio selection loaded from the current token-fenced `ClipDetail`;
  delete-local-after-upload remains an existing saved Cloud setting, not a new per-upload control.
- Route callbacks to typed controller actions. Post effects to bounded workers; post completions
  through `invoke_from_event_loop` with weak handles and exact tokens. No callback performs I/O.
- Pin keyboard behavior: Tab/Shift+Tab visible focus, arrows move the card focus within the page,
  Enter opens/accepts, Space selects in select mode, Context Menu/Shift+F10 opens the menu,
  Ctrl+A selects the visible page, PageUp/PageDown change pages, Escape priority is deterministic,
  and destructive confirmation cannot be triggered by key repeat.
- Give every custom control an accessibility role, name, value/state, checked/selected/expanded
  state, focus affordance, and logical order. Add structural/UI Automation contract tests where
  supported; keep Narrator and high-DPI verification explicitly manual.
- Window drop synchronously detaches catalog projection, releases all Slint images and scoped cloud
  media lease, cancels window-scoped work, and leaves durable uploads/controller state alive.
- Extend the current fixture-bound `LiveSession` with bounded dynamic Open/Replace/Close commands for
  validated local or cached-cloud sources. Transfer an opaque source lease into the accepted Open;
  release the prior file/decoder/audio/source lease before publishing replacement readiness. Stale
  open results release their incoming lease, and fixture startup remains a harness-only path.

**Implement to green**

- Use Slint `ModelRc` only for the active page and bounded summaries. Prefer shared immutable models
  and row updates over rebuilding for byte-progress-only events.
- Use native file-backed `slint::Image` values. Keep the existing gradient placeholder for missing,
  queued, failed, or explicitly unavailable posters.
- Preserve the reserved native Review video child; opening a row hands a validated local/cached path
  and scoped lease to the existing native playback session.

## Task 10: Rebase the shipping Tauri Library/Cloud adapters on the shared services

**Files**

- Modify: `apps/clipline-app/src/app.rs`
- Modify: `apps/clipline-app/src/library.rs`
- Modify: `apps/clipline-app/src/cloud.rs`
- Modify: `apps/clipline-app/src/cloud_upload.rs`
- Modify: `apps/clipline-app/src/settings/mod.rs`
- Modify: `apps/clipline-app/tests/ui_contract.rs`
- Modify: `apps/clipline-app/tests/command_contract.rs`
- Modify: `apps/clipline-app/tests/repository_security.rs`
- Modify: relevant Library/Cloud/desktop tests

**Test first**

- Run old JavaScript, command JSON, UI contract, cloud HTTP/cache/upload, Library scan/mutation,
  recorder-save refresh, and settings transaction tests unchanged against the shared service.
- Assert command registrations/names, camelCase arguments, serialization, Tauri asset scope,
  emitted progress payloads, status strings, foreground notices, and error behavior remain exact.
- Prove the Tauri compatibility adapter can still collect its legacy complete Cloud list within the
  existing 10,000 limit while Slint uses server pages.
- Prove both adapters share account-fenced upload persistence and active-file leases. Add a
  disconnect/reconnect regression at the Tauri boundary.
- Repository tests reject duplicate scan/poster/cloud/upload algorithms in the spike/app adapter
  and prevent UI frameworks from entering the shared crate.

**Implement to green**

- Leave HTML/CSS/JS and Boa tests in place as a rollback adapter until Milestone 10.
- Keep the Tauri production package and user profile untouched; this task is an internal dependency
  inversion, not a shell cutover.

## Task 11: Prove large-library, poster-process, lifecycle, and cloud gates

**Files**

- Create: `scripts/measure-slint-library.ps1`
- Create: `apps/clipline-slint-spike/examples/catalog_harness.rs`
- Create: `docs/slint/native-library-protocol.md`
- Modify: `scripts/measure-frontend-baseline.ps1`
- Modify: `apps/clipline-slint-spike/tests/spike_contract.rs`

**Test first**

- Extend the isolated-profile fixture builder to create exactly 50, 500, and 2,000 hash-verified
  hard-linked clips without touching the installed profile. Record source hash, count, path,
  build SHA, renderer, GPU/adapter, scale, and process identity in every result.
- The harness exposes exact ready/page-settled/posters-settled/error/stop markers and atomic JSON.
  The sampler verifies PID + creation time, excludes unrelated Clipline processes instead of
  killing/aborting on them, and cleans only its owned process/profile tree.
- Measure first usable page latency, page-change p95, filter/group p95, retained row/image counts,
  poster cache size, FFmpeg child peak, duplicate extraction count, private working set p50/p95/max,
  CPU, handle count, and reveal/close deltas. Sample local cold posters, local warm posters, Cloud
  synthetic pages, selection/page churn, and open/close cycles.
- Absolute gates: rows <= 60, decoded images <= 32, poster path/status LRU <= 120, FFmpeg peak <= 2, no duplicate same-key
  extraction, no off-page image retention after settle, no unbounded queue/cache/handle growth,
  first usable 2,000-clip page <= 1.5 s on the gate machine, and 2,000-clip open-Library private
  working-set five-minute p50 <= 140 MiB. Record p95/max as diagnostics. Record machine/noise
  rejection rules; do not reinterpret a rejected run as passing evidence.
- Matched gates: Slint open-Library five-minute PWS p50 <= 65% of matched optimized Tauri and private
  commit at least 25% lower; page/filter p95 no worse than Tauri by more than 50 ms, and CPU within
  one percentage point over equal windows.
  If installed Clipline or environment noise blocks Tauri matching, keep the matched gate pending
  rather than inventing a result.
- Run 100 Local/Cloud page switches, poster cancellations, window reveal/close cycles, cloud-media
  open/replace/close cycles, and upload-progress bursts. Require balanced attachment/image/lease/temp
  counters, zero stale publications, zero active lease after close, and <=10 MiB PWS growth.

**Implement to green**

- Reuse `Clipline.ProcessMetrics.psm1`, baseline provenance rules, create-new evidence paths, and
  the existing Slint shell driver contract.
- Gate media/cloud network cases on explicit fixtures or a test server; never send credentials,
  mutate production Cloud data, or call the real account during automated runs.
- Run at least three accepted samples for each absolute scenario before marking it passed. Manual
  Narrator, UI Automation, high-DPI, and real-account smoke remain named pending gates if unavailable.

## Task 12: Close Milestone 7 without overstating parity

**Files**

- Modify: `docs/slint/parity-ledger.md`
- Modify: `docs/slint/native-library-protocol.md`
- Modify: `handoff.md`
- Modify: `ddoc.md` only for durable architecture decisions proven in this milestone

**Test and validation**

- Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
- Clean each changed crate and rerun warning-denied all-target Clippy to catch warm-cache misses.
- Run the standalone Slint package tests and warning-denied all-target Clippy for normal,
  `package-regular`, and `package-standalone` feature sets.
- Run focused JavaScript cross-implementation, Tauri command/UI contract, repository-security,
  cloud/cache/upload, Library mutation, desktop adapter, Slint compile/keyboard, and measurement-
  helper suites.
- Run accepted 50/500/2,000 absolute samples where the environment permits. Keep matched Tauri,
  real-account, Narrator/UI Automation, high-DPI, Win10/Win11, and real-GPU gaps explicitly pending.
- Record exact commits, fixture hashes, raw evidence locations, renderer/adapter, accepted/rejected
  sample counts, bounds, skips, and blockers. Advance ledger rows only for behavior actually proven.
- Commit each green logical slice conventionally, then push/refresh draft PR #132 when GitHub OAuth
  has `workflow` scope. A push failure must not stop local milestone work.

## Required completion evidence

Milestone 7 is complete only when:

1. the neutral Rust cross-implementation corpus matches the retained JavaScript contracts;
2. Slint presents no more than 60 local/cloud rows and 32 decoded page images, with selection/active identity
   surviving window recreation in the controller;
3. local scan/rename/delete/reveal, poster extraction, cloud page/thumbnail/media/profile/open/share,
   upload progress/completion/status sync, account fencing, foreground feedback, and scoped media
   leases all run through shared services rather than Tauri/spike forks;
4. old Tauri Library/Cloud contract tests remain green and the production adapter remains shippable;
5. absolute large-library/process/lifecycle gates pass or the milestone records the mandated no-go;
   matched/manual/environment-dependent gates may remain explicitly pending but never silently waived;
6. workspace and standalone test/Clippy gates are green, handoff/protocol/ledger are current, and no
   installed Clipline state or production Cloud account was touched.
