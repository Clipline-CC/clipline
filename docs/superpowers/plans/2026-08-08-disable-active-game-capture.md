# Disable Active Game Capture Plan

## Goal

When a user disables the game Clipline is currently capturing and saves Settings, end that capture immediately and keep it disabled on later detection passes.

## Root cause

Supported games can also exist as auto-detected custom games. Disabling the supported profile clears the active capture, but the detector can immediately match the same window through the duplicate custom rule. Custom games can also be added more than once under different IDs; disabling one exact duplicate leaves another enabled. A detection result produced just before the settings save can arrive afterward and restore the disabled game.

## Tasks

1. Add a regression test proving that a disabled supported game cannot be matched through a duplicate custom rule.
2. Reserve windows recognized by supported profiles for those profiles, whether the profile is enabled or disabled.
3. Revalidate each detector result against the current settings before changing the active game.
4. Normalize exact custom-game duplicates so the newest entry wins, repairing existing settings and preventing persistence from every add path.
5. Prevent Add Custom Game from presenting an already configured executable as a new entry.
6. Run the focused regressions, workspace tests, and clippy; then relaunch Clipline for manual verification.
