# Player header metadata and timeline marker filters

Base: `develop` at `a3885f12`. User request: move League champion/spells/KDA/items
beside the clip title/file details and allow timeline marker categories to be toggled,
including a bookmarks-only view.

- [ ] Add focused behavioral/DOM regressions before implementing new filtering.
- [ ] Move the existing metadata panel into the player header. Keep its existing
  profile/settings policy, icon fallbacks and safe text rendering. Lay out the title
  and metadata side by side with wrapping on narrow windows; remove the vacated grid row.
- [ ] Add a native checkbox category menu beside timeline controls, with Show all,
  Hide all and Bookmarks only actions. Derive options/counts from available markers,
  keep the control reachable when every marker is hidden, and default to all visible.
- [ ] Keep the viewer's hidden-category set across clip switches within this webview.
  Apply it after existing game-review filters/bookmark merging in a separate timeline
  accessor. Gallery marker counts/filters, match-event rails and saved marker data
  continue using their existing accessors. Route player pins, counts, marker/edit
  navigation and snapping through the filtered timeline accessor.
- [ ] Reuse DOM-free category/filter helpers in `player-core.js`; reuse standard
  checkbox/details controls and existing theme styling. Add no dependencies/settings schema.
- [ ] Obtain independent read-only review, run workspace tests and fresh-cache
  warning-denied Clippy, and check the live layout/filter interactions.
- [ ] Update `handoff.md`, reopen the patched app, and open a PR into `develop`
  with passing Ubuntu/Windows CI.

Plan checkboxes remain unticked by repository convention.
