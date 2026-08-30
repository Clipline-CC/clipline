# Integrated Groups Review Follow-up

**Goal:** Keep generated group media recoverable and make integrated group cards obey every existing
Library grouping and sort control.

- [ ] Add failing UI contracts for exposing stale/orphaned compilations, deriving deterministic
      group game/session metadata, and counting member markers for Most markers sorting.
- [ ] Hide only the exact current compilation selected by a live group's fingerprint; render stale,
      duplicate, legacy, and orphaned generated outputs as ordinary Compilation cards.
- [ ] Project each group into one game/session bucket using the member that supplies its latest
      modified timestamp, preserving one-card pagination for mixed groups.
- [ ] Sum member marker counts when sorting integrated group cards by Most markers.
- [ ] Run focused contracts, workspace tests, and warning-denied Clippy; update the design/handoff,
      push the PR, and resolve the review threads.
