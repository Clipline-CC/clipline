# Public Clip Share URL

## Goal

Use Clipline Cloud's canonical public share URL for public and unlisted clips, never expose the
authenticated owner route as a share link, and clear saved share state whenever a clip is private.

## Implementation

- [ ] Add native regressions proving `ClipDetailResponse.public_url` is persisted verbatim and a
  null `public_url` clears any previously saved share URL.
- [ ] Stop synthesizing `/clip/{clip_id}` into upload progress, upload records, and Cloud-library
  summaries; retain that owner route only for the explicit authenticated “Open cloud page” action.
- [ ] After a visibility update, fetch `GET /api/v1/clips/{clip_id}` and apply the refreshed detail
  response rather than trusting or reconstructing share state.
- [ ] Treat remote clip identity and public shareability independently so private clips stay in the
  Cloud library, cannot be uploaded twice, and expose no copy-link action.
- [ ] Clear legacy locally synthesized owner URLs during settings normalization.
- [ ] Update the UI contracts and DOM-free library tests for public, unlisted, private, processing,
  and visibility-transition behavior.
- [ ] Record the share-URL invariant and compatibility migration in `handoff.md`.

## Verification

- [ ] Run focused native Cloud, settings, player-core, and UI contract tests.
- [ ] Run `cargo test --workspace`.
- [ ] Run `cargo clean -p clipline-app` and
  `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Launch Clipline and verify public/unlisted copy the API-provided `/c/c_...` URL, private clips
  offer only the authenticated open-page action, and visibility refreshes update or clear the link.
- [ ] Paste a copied public URL into Discord and verify its title/poster embed against a live Cloud
  deployment.
