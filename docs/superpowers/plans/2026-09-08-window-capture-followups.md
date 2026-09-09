# Remaining window-capture controls

- [ ] Test the untested DWM thumbnail-host plus PrintWindow flags=2 combination
  with an ordinary windowed flip mock. Preserve host/source identity, affinity,
  geometry and live-view evidence. Bound the worker externally. If only the host
  background is readable, stop this branch without fullscreen expansion.
- [ ] Run the existing DWM probe against the copied mock with FSO disabled, with
  a bounded PID-scoped PresentMon trace to classify presentation concurrently.
  Preserve BMPs, motion failure, actual presentation mode and cleanup evidence.
- [ ] Restore the copied fixture compatibility value, reopen Clipline, document
  results and limitations, update the existing PR and verify CI.

No WGC, injection, protection bypass, monitor fallback, global settings change,
or experimental production-backend integration. Use existing tested probes;
new implementation work requires reproduced defects or a passing feasibility test.
