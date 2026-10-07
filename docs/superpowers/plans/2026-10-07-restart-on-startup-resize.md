# Restart recording when a game resizes during startup

Base: `develop` at `df7fb0b0`. On Windows 11 the encoder size comes from the
first captured frame and every later frame is stretched to fill it. Games
such as Abiotic Factor and Ready or Not open fullscreen and then switch to
the user's window size, so the whole recording is upscaled or distorted.
The osu! profile avoids its startup window by title, but these games reuse
one window, so a title rule can't help.

Decision (Dain, 2026-10-07): treat a size change in the first seconds of a
recording as part of the game's startup and restart there. Later resizes
keep today's behaviour for now.

- [ ] Add a failing pure policy test for `StartupResizeWatch`: no restart
  while the size matches the encoder; restart once a different size has
  held for 1 s, if the change was first seen within 10 s of the run's first
  frame; a change first seen later never restarts; flapping sizes restart
  the settle timer; returning to the encoder size cancels; a disarmed watch
  never restarts.
- [ ] Arm the watch only for stretch-fit sources (WGC window and full
  display capture), never for the Windows 10 fallback (already letterboxed)
  or regions. Feed it each captured frame's texture size.
- [ ] On restart, throw the startup away: finish and delete the short
  full-session file and its ownership marker, sweep an emptied session
  folder, drop the replay buffer, and start a fresh run (no recovery pass)
  on the same command channel and options. Clips the user already saved
  stay. At most 3 restarts per recorder start. Log the old and new sizes.
- [ ] Verify with a test window that opens large and shrinks: the restarted
  recording has the final size, no leftover session file, and a stop or
  save during the restart behaves.
- [ ] Run workspace tests and fresh-cache warning-denied Clippy on Rust 1.98
  and 1.99, update the handoff, and create a PR into develop with green OS
  checks.

Plan checkboxes remain unticked by repository convention.
