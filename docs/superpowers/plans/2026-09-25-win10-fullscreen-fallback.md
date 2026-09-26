# Windows 10 fullscreen fallback capture

Make Automatic choose the Windows 10 fallback on Windows 10 and WGC on Windows 11.
The explicit choices are Default (Windows 11), which uses WGC, and Fallback
mode, which uses WGC for an ordinary game window and Desktop Duplication for
that game's fullscreen monitor. Display and region targets use Desktop
Duplication in fallback mode. Existing `wgc` and `desktop_duplication` settings
load as Default and Fallback respectively.

- [ ] Add failing neutral policy tests for OS routing, source selection,
      foreground and monitor guards, and legacy setting values.
- [ ] Implement a Windows fullscreen observer and capture switcher. Require the
      selected window's client area to cover its monitor and the same window to
      be foreground before opening Desktop Duplication. Release WGC before the
      switch and discard a frame if the guard changes during acquisition.
- [ ] Keep a safe black transition frame so the cadence layer cannot repeat a
      prior desktop image while the selected game is not foreground or while
      a new source starts. Preserve the shared D3D device and capture clock.
- [ ] Update the three settings choices, descriptions, design document, and
      handoff. Test a windowed-to-fullscreen-to-windowed game transition on
      Windows 10, then run workspace tests and warning-denied Clippy.
- [ ] Stop the previous app, build and launch the updated app for user testing.

Checkboxes stay unticked (repo convention).
