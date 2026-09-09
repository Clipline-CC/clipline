# Startup display label correction

Live reproduction: after restarting Clipline with a valid named full display,
the rail tooltip says unavailable before the settings display list is loaded.
Windows still enumerates the saved ID and recording starts successfully.

- [ ] Use the saved display identity for the fallback rail label when the display
  cache has no matching friendly name. Do not infer availability from that cache.
- [ ] Rebuild, restart and inspect the tooltip before opening Settings; then load
  Settings and check the friendly display name. This is a wording-only correction.
- [ ] Run required workspace gates, update handoff, push and check CI.
