# PR #144 review fixes

## Goal

Make the Light theme's themed controls readable, make the capture glow follow the selected accent,
and remove drift from the palette contract tests without adding theme-specific component branches.

## Steps

- [ ] Add failing UI-contract coverage for semantic danger/trim colors, an accent-driven capture
      glow, and consistent clip-kind assertions across every alternate palette.
- [ ] Replace dark-only control literals with the existing semantic palette tokens and use the
      selected accent for the active capture glow.
- [ ] Consolidate the repeated theme serde and CSS contract helpers so new palettes inherit the
      same checks.
- [ ] Record the review hardening in `handoff.md`, then run workspace tests and clean-cache clippy.

