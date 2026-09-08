# Explicit display selection and strict exclusive capture feasibility

- [ ] Persist full-display intent and selected display ID separately from fixed
  region geometry. Legacy primary/region settings keep their meaning.
- [ ] Add failing neutral settings/UI-selection tests for round trips, unchanged
  regions, primary selection, missing display preservation and source routing.
- [ ] Route selected full displays to monitor capture without fixed crop; keep
  missing selections as errors rather than choosing another display silently.
- [ ] Label monitor and region choices clearly, retain custom region bounds, and
  validate Save/restart/fullscreen/audio/replay on the Windows 10 AMD machine.
- [ ] Investigate remaining strict game-only exclusive APIs using primary sources
  and bounded diagnostics if a credible untested path exists. Preserve rejection
  if source isolation cannot be guaranteed; never relabel monitor capture game-only.
- [ ] Review changes, run CI=1 workspace tests and warning-denied Clippy, rebuild
  and reopen the app, preserve evidence, update handoff/PR and verify both CI OSes.

No WGC fallback/diagnostic, injection, protected-content bypass, driver installation,
GPL capture implementation or production integration of an unvalidated experiment.