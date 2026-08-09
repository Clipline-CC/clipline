# League Game Type Filter Plan

**Goal:** Tag new League recordings with their queue type and let users filter the local
library by familiar modes such as Ranked Solo/Duo, Normal, and ARAM.

## Product decisions

- Read the active queue from League's local client only. No Riot account link, remote API key,
  or cloud lookup is required.
- Store the raw queue ID plus a stable category and friendly label in the existing
  `clipline-session.json` document. Every clip in a detected match already shares that sidecar.
- Group the common queues as Ranked Solo/Duo, Ranked Flex, Normal, ARAM, Arena, Custom, and
  Other. Preserve a more specific label such as Normal Draft or Quickplay for display.
- Show the queue label on League library cards and expose an independent game-type selector that
  composes with the existing clip-kind, search, sort, and group controls.
- Older clips and sessions recorded while the League client lookup is unavailable remain
  `Unknown`. Queue detection is enrichment and must never interrupt capture or saving.

## Minimal implementation

Reuse the existing League poller and per-match session lifecycle. At match start, read the League
client lockfile derived from the detected game executable, make one authenticated loopback request
for the gameflow session, and send the resulting queue tag through the existing poller channel.
The recorder merges that tag into the current session sidecar, including a full-session folder
that may have been created before the in-game API became ready.

Do not add a database, background index, remote match-history lookup, or configurable queue
catalog. A small explicit map covers the durable user-facing categories while retaining the raw
queue ID for future remapping.

## Plan-driven implementation

### Task 1: Lock queue parsing and categorization with failing tests

- [ ] Add tests for League lockfile parsing and loopback-only LCU client construction.
- [ ] Add a mock-server test for extracting `gameData.queue.id` from the gameflow session.
- [ ] Add table tests for common queue IDs and the unknown fallback.

### Task 2: Capture queue metadata without affecting recording

- [ ] Preserve the detected game executable path through runtime service options.
- [ ] Have the League poller attempt one queue lookup per detected match and emit a queue message.
- [ ] Merge queue metadata into the existing session sidecar without losing its game identity.
- [ ] Keep lookup, parsing, and write failures warning-only.

### Task 3: Expose the metadata in the library

- [ ] Deserialize optional queue metadata with backward compatibility for existing sidecars.
- [ ] Add an independent League game-type selector that is hidden when no categorized League
  recordings are present.
- [ ] Filter by the stable queue category, include queue text in search, and display the friendly
  queue label on cards.
- [ ] Add focused UI contract coverage for selector wiring and composition with existing filters.

### Task 4: Verify and hand off

- [ ] Run focused crate and UI tests while iterating.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings` with a clean cache for changed
  crates.
- [ ] Update `handoff.md`, commit the implementation, and relaunch Clipline for manual testing.
