# Integrated Groups Library TDD Plan

**Goal:** Present groups as ordinary Library items and keep their generated compilation inside the
group instead of rendering it as a second clip.

- [ ] Add a failing UI contract requiring the Groups filter beside Has markers, generated group
      compilations excluded from top-level clips, and group cards rendered inside normal buckets.
- [ ] Move the Groups filter chip beside Has markers.
- [ ] Merge visible groups into the normal sort/group/page pipeline and route each item to its
      existing card renderer, deleting the standalone Groups heading.
- [ ] Hide `source_group` compilation clips from the top-level local Library while retaining them
      for group copy/upload/reuse.
- [ ] Run focused UI contracts, workspace tests, and warning-denied Clippy; open Clipline for manual
      verification and update `handoff.md`.
