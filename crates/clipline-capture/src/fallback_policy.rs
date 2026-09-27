//! Neutral source policy for windowed WGC / fullscreen Desktop Duplication.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Window,
    Display,
    Waiting,
}

/// Whether the game's monitor can still be showing only the game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Focus {
    /// A window of the game's own process is foreground.
    Game,
    /// Another app is foreground on a different monitor.
    OtherMonitor,
    /// Another app, or nothing, is foreground on the game's monitor.
    Covered,
}

/// Screen-space rectangle, right/bottom exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl ScreenRect {
    fn overlaps(self, other: Self) -> bool {
        self.left < other.right
            && other.left < self.right
            && self.top < other.bottom
            && other.top < self.bottom
    }
}

/// Process ownership, not the exact HWND: some games focus a sibling window
/// of the one selected for capture. Another app's window must stay entirely
/// off the game's monitor, because duplication would record any overlap.
pub(crate) fn classify_focus(
    game_process_foreground: bool,
    foreground_bounds: Option<ScreenRect>,
    game_monitor: ScreenRect,
) -> Focus {
    if game_process_foreground {
        Focus::Game
    } else if foreground_bounds.is_some_and(|bounds| !bounds.overlaps(game_monitor)) {
        Focus::OtherMonitor
    } else {
        Focus::Covered
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Observation {
    pub available: bool,
    pub focus: Focus,
    pub covers_monitor: bool,
    /// False once Desktop Duplication rejected this monitor as unsupported,
    /// e.g. a rotated display or one driven by another GPU.
    pub display_supported: bool,
}

pub(crate) fn choose(observation: Observation) -> Source {
    if !observation.available {
        Source::Waiting
    } else if !observation.covers_monitor || !observation.display_supported {
        Source::Window
    } else if observation.focus == Focus::Covered {
        Source::Waiting
    } else {
        Source::Display
    }
}

pub(crate) fn accept_frame(
    source: Source,
    before: Observation,
    after: Observation,
    same_monitor: bool,
) -> bool {
    source != Source::Waiting
        && same_monitor
        && before == after
        && choose(before) == source
        && choose(after) == source
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windowed() -> Observation {
        Observation {
            available: true,
            focus: Focus::Game,
            covers_monitor: false,
            display_supported: true,
        }
    }

    fn fullscreen() -> Observation {
        Observation {
            covers_monitor: true,
            ..windowed()
        }
    }

    #[test]
    fn unsupported_displays_keep_fullscreen_games_on_wgc() {
        let unsupported = Observation {
            display_supported: false,
            ..fullscreen()
        };
        assert_eq!(choose(unsupported), Source::Window);
        assert_eq!(
            choose(Observation {
                focus: Focus::Covered,
                ..unsupported
            }),
            Source::Window
        );
        assert!(accept_frame(Source::Window, unsupported, unsupported, true));
    }

    #[test]
    fn ordinary_windows_stay_on_wgc_even_in_the_background() {
        assert_eq!(choose(windowed()), Source::Window);
        for focus in [Focus::OtherMonitor, Focus::Covered] {
            assert_eq!(
                choose(Observation {
                    focus,
                    ..windowed()
                }),
                Source::Window
            );
        }
    }

    #[test]
    fn fullscreen_switch_requires_an_uncovered_game_monitor() {
        assert_eq!(choose(fullscreen()), Source::Display);
        assert_eq!(
            choose(Observation {
                focus: Focus::Covered,
                ..fullscreen()
            }),
            Source::Waiting
        );
        assert_eq!(
            choose(Observation {
                available: false,
                ..fullscreen()
            }),
            Source::Waiting
        );
    }

    #[test]
    fn focus_on_another_monitor_keeps_the_fullscreen_game_recording() {
        assert_eq!(
            choose(Observation {
                focus: Focus::OtherMonitor,
                ..fullscreen()
            }),
            Source::Display
        );
    }

    const PRIMARY: ScreenRect = ScreenRect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };

    #[test]
    fn focus_classifies_process_and_foreground_bounds() {
        let right_monitor = ScreenRect {
            left: 1920,
            top: 0,
            right: 3840,
            bottom: 1080,
        };
        let straddling = ScreenRect {
            left: 1800,
            ..right_monitor
        };
        assert_eq!(classify_focus(true, None, PRIMARY), Focus::Game);
        assert_eq!(classify_focus(true, Some(PRIMARY), PRIMARY), Focus::Game);
        // Touching edges share no pixel; right/bottom are exclusive.
        assert_eq!(
            classify_focus(false, Some(right_monitor), PRIMARY),
            Focus::OtherMonitor
        );
        assert_eq!(
            classify_focus(false, Some(straddling), PRIMARY),
            Focus::Covered
        );
        assert_eq!(classify_focus(false, Some(PRIMARY), PRIMARY), Focus::Covered);
        assert_eq!(classify_focus(false, None, PRIMARY), Focus::Covered);
    }

    #[test]
    fn frames_are_discarded_when_a_guard_or_monitor_changes() {
        let fullscreen = fullscreen();
        assert!(accept_frame(Source::Display, fullscreen, fullscreen, true));
        assert!(!accept_frame(
            Source::Display,
            fullscreen,
            fullscreen,
            false
        ));
        assert!(!accept_frame(
            Source::Display,
            fullscreen,
            Observation {
                focus: Focus::Covered,
                ..fullscreen
            },
            true
        ));
        assert!(!accept_frame(Source::Display, fullscreen, windowed(), true));
        assert!(!accept_frame(Source::Window, windowed(), fullscreen, true));
    }
}
