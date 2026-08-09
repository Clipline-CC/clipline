# Disable Active Game Capture Plan

## Goal

When a user disables the game Clipline is currently capturing and saves Settings, end that capture immediately and keep it disabled on later detection passes.

## Root cause

Supported games can also exist as auto-detected custom games. Disabling the supported profile clears the active capture, but the detector can immediately match the same window through the duplicate custom rule. A detection result produced just before the settings save can also arrive afterward and restore the disabled game.

## Tasks

1. Add a regression test proving that a disabled supported game cannot be matched through a duplicate custom rule.
2. Reserve windows recognized by supported profiles for those profiles, whether the profile is enabled or disabled.
3. Revalidate each detector result against the current settings before changing the active game.
4. Run the focused regression, workspace tests, and clippy; then relaunch Clipline for manual verification.

