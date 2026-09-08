# Fullscreen surface continuity at unchanged resolution

Test the experimental window-capture paths themselves. Do not substitute display
capture, WGC, injection, a cooperating frame-export fixture, or production code.

- [ ] Use the existing byte-identical flip mock with a new copied executable
  identity. Temporarily disable FSO only for that copy, restoring its prior value
  in supervised cleanup. Establish a 1280x720 borderless positive control first.
- [ ] Enter exclusive from that borderless state. Record window/client/monitor
  geometry and source identity throughout. Reject the unchanged-resolution claim
  if dimensions change. Correlate exclusive sampling with bounded, user-authorized
  signed PresentMon and require Legacy Flip before calling it native-exclusive.
- [ ] Read the source directly with existing DWM and PrintWindow probes. Save
  images and counter evidence, then return to borderless and repeat the positive
  control. Separate API success from useful, fresh game pixels.
- [ ] If unchanged-resolution exclusive works, repeat and inspect sustained
  freshness, colors and border behavior before expanding the recorder. Otherwise
  document that the failed mechanism is not explained solely by display resizing.
- [ ] Restore fixture compatibility settings, stop test helpers, preserve local
  evidence, update documentation/PR, and verify CI. Keep Clipline paused.

This is a previously unisolated geometry/presentation control, not a promised fix
or a reinterpretation of the earlier classified 720x480 failures.
