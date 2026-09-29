# Honor explicit border-free display capture

Windows 10 AMD validation reproduced a misleading production contract: explicit
Desktop Duplication permits WGC fallback, and detected window sources skip DXGI.
The user's no-yellow-border requirement supersedes the historical silent fallback
preference. DWM remains a standalone experiment; this change does not supply
isolated game recording or change Auto/WGC defaults.

- [ ] Extract the backend decision into a neutral module used by production.
  Add failing tests that prohibit invoking WGC after explicit DXGI errors or for
  incompatible window sources; preserve Auto/WGC success and failure behavior.
- [ ] Make explicit Desktop Duplication return its construction/first-frame error
  directly and reject window sources with an actionable display-capture message.
  Update the settings explanation and source comments.
- [ ] Run workspace tests and warning-denied Clippy, build and launch Clipline.
  Validate AMD display recording, replay save/playback, and record audio limits.
- [ ] Preserve AMD DWM fixture/WebGL evidence separately from app recording;
  document game/fullscreen/synchronization prerequisites without claiming completion.
  Update handoff, obtain a second code review, push and inspect Windows/Linux CI.
