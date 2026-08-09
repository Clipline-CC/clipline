# Icon-Shaped Capture Glow Plan

## Goal

Make the active capture glow follow the visible game, monitor, or region icon instead of drawing a fixed rounded square around every icon.

## Tasks

1. Update the rail UI contract to require an alpha-aware icon filter and no active container shadow.
2. Move the active blue glow from `.rail-game` to its rendered image or placeholder icon.
3. Run the focused UI contract, workspace tests, and clippy; then relaunch Clipline for visual verification.

