//! Neutral source policy for windowed WGC / fullscreen Desktop Duplication.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Window,
    Display,
    Waiting,
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

    fn contains(self, other: Self) -> bool {
        self.left <= other.left
            && self.top <= other.top
            && self.right >= other.right
            && self.bottom >= other.bottom
    }
}

/// Some games size their client a few pixels past the monitor edges, so
/// containment counts as fullscreen. An overhang that reaches another
/// monitor does not: duplication would drop the game's content there.
/// `monitors` is only consulted for an overhang; `None` means enumeration
/// failed, which fails closed.
pub(crate) fn client_covers_monitor(
    client: ScreenRect,
    monitor: ScreenRect,
    monitors: impl FnOnce() -> Option<Vec<ScreenRect>>,
) -> bool {
    if client == monitor {
        return true;
    }
    client.contains(monitor)
        && monitors().is_some_and(|monitors| {
            monitors
                .iter()
                .all(|&other| other == monitor || !client.overlaps(other))
        })
}

/// One top-level window stacked above the game. Callers may leave the later
/// fields at their defaults once `same_process` or `!shown` already rules the
/// window out, so the costlier queries only run for real candidates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WindowAbove {
    /// Owned by the game's process, e.g. a sibling of the captured window.
    pub same_process: bool,
    /// Visible, not minimized, and not cloaked by DWM.
    pub shown: bool,
    /// A layered, click-through overlay (WS_EX_LAYERED | WS_EX_TRANSPARENT).
    pub click_through: bool,
    /// Display affinity keeps it out of duplication (black or omitted).
    pub excluded_from_capture: bool,
    /// The shell taskbar, which reappears over a game that loses focus.
    pub taskbar: bool,
    /// Visible DWM bounds; `None` if they could not be read.
    pub bounds: Option<ScreenRect>,
}

/// Duplication records the whole monitor, so another app's window stacked
/// above the game and reaching its monitor would enter the recording. That
/// includes always-on-top windows and windows shown without taking focus.
/// Click-through overlays and the taskbar are recorded as they appear.
/// Unreadable bounds fail closed. Windows 10 draws toasts and system overlays
/// outside the window list, so they still reach the recording.
fn window_covers_monitor(window: WindowAbove, monitor: ScreenRect) -> bool {
    !window.same_process
        && window.shown
        && !window.click_through
        && !window.excluded_from_capture
        && !window.taskbar
        && window.bounds.is_none_or(|bounds| bounds.overlaps(monitor))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StackCover<T> {
    Clear,
    By(T),
    /// More windows above the game than the walk allows; fails closed.
    Unknown,
}

impl<T> StackCover<T> {
    pub(crate) fn is_clear(&self) -> bool {
        matches!(self, Self::Clear)
    }
}

/// Walk the windows above the game, nearest first, and report the first one
/// that would enter a duplication of the game's monitor.
pub(crate) fn first_cover<T>(
    stack: impl IntoIterator<Item = (T, WindowAbove)>,
    monitor: ScreenRect,
    max_windows: usize,
) -> StackCover<T> {
    for (index, (id, window)) in stack.into_iter().enumerate() {
        if index >= max_windows {
            return StackCover::Unknown;
        }
        if window_covers_monitor(window, monitor) {
            return StackCover::By(id);
        }
    }
    StackCover::Clear
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Observation {
    pub available: bool,
    pub covers_monitor: bool,
    /// Another app's window above the game reaches the game's monitor.
    pub covered: bool,
    /// False once Desktop Duplication rejected this monitor as unsupported,
    /// e.g. a rotated display or one driven by another GPU.
    pub display_supported: bool,
}

pub(crate) fn choose(observation: Observation) -> Source {
    if !observation.available {
        Source::Waiting
    } else if !observation.covers_monitor || !observation.display_supported {
        Source::Window
    } else if observation.covered {
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
            covers_monitor: false,
            covered: false,
            display_supported: true,
        }
    }

    fn fullscreen() -> Observation {
        Observation {
            covers_monitor: true,
            ..windowed()
        }
    }

    const PRIMARY: ScreenRect = ScreenRect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };

    const RIGHT_MONITOR: ScreenRect = ScreenRect {
        left: 1920,
        top: 0,
        right: 3840,
        bottom: 1080,
    };

    fn popup() -> WindowAbove {
        WindowAbove {
            shown: true,
            bounds: Some(ScreenRect {
                left: 1540,
                top: 900,
                right: 1900,
                bottom: 1040,
            }),
            ..WindowAbove::default()
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
                covered: true,
                ..unsupported
            }),
            Source::Window
        );
        assert!(accept_frame(Source::Window, unsupported, unsupported, true));
    }

    #[test]
    fn ordinary_windows_stay_on_wgc_even_when_covered() {
        assert_eq!(choose(windowed()), Source::Window);
        assert_eq!(
            choose(Observation {
                covered: true,
                ..windowed()
            }),
            Source::Window
        );
    }

    #[test]
    fn fullscreen_switch_requires_an_uncovered_game_monitor() {
        assert_eq!(choose(fullscreen()), Source::Display);
        assert_eq!(
            choose(Observation {
                covered: true,
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
    fn a_client_that_fills_or_overhangs_only_its_monitor_covers_it() {
        let no_enumeration = || -> Option<Vec<ScreenRect>> {
            panic!("an exact match must not enumerate monitors")
        };
        assert!(client_covers_monitor(PRIMARY, PRIMARY, no_enumeration));

        let overhang = ScreenRect {
            left: -2,
            top: -2,
            right: 1922,
            bottom: 1082,
        };
        assert!(client_covers_monitor(overhang, PRIMARY, || Some(vec![
            PRIMARY
        ])));
        assert!(!client_covers_monitor(overhang, PRIMARY, || Some(vec![
            PRIMARY,
            RIGHT_MONITOR
        ])));
        assert!(!client_covers_monitor(overhang, PRIMARY, || None));

        let short = ScreenRect {
            bottom: 1079,
            ..PRIMARY
        };
        assert!(!client_covers_monitor(short, PRIMARY, || Some(vec![
            PRIMARY
        ])));
    }

    #[test]
    fn an_always_on_top_window_over_the_game_covers_it_without_focus() {
        assert_eq!(first_cover([(7, popup())], PRIMARY, 16), StackCover::By(7));
    }

    #[test]
    fn intentional_exceptions_and_undrawn_windows_do_not_cover() {
        // Hidden and capture-excluded windows never reach a duplication. The
        // game's own windows, click-through overlays and the taskbar do; they
        // are deliberate exceptions and are recorded as they appear.
        let ignored = [
            WindowAbove {
                same_process: true,
                ..popup()
            },
            WindowAbove {
                shown: false,
                ..popup()
            },
            WindowAbove {
                click_through: true,
                ..popup()
            },
            WindowAbove {
                excluded_from_capture: true,
                ..popup()
            },
            WindowAbove {
                taskbar: true,
                ..popup()
            },
        ];
        assert_eq!(
            first_cover(ignored.into_iter().enumerate(), PRIMARY, 16),
            StackCover::Clear
        );
    }

    #[test]
    fn a_focused_app_on_another_monitor_does_not_cover() {
        let browser = WindowAbove {
            bounds: Some(RIGHT_MONITOR),
            ..popup()
        };
        let straddling = WindowAbove {
            bounds: Some(ScreenRect {
                left: 1800,
                ..RIGHT_MONITOR
            }),
            ..popup()
        };
        // Touching edges share no pixel; right/bottom are exclusive.
        assert!(first_cover([(0, browser)], PRIMARY, 16).is_clear());
        assert_eq!(
            first_cover([(1, straddling)], PRIMARY, 16),
            StackCover::By(1)
        );
    }

    #[test]
    fn unreadable_bounds_and_deep_stacks_fail_closed() {
        let unknown_bounds = WindowAbove {
            bounds: None,
            ..popup()
        };
        assert_eq!(
            first_cover([(3, unknown_bounds)], PRIMARY, 16),
            StackCover::By(3)
        );
        let hidden = WindowAbove::default();
        assert_eq!(
            first_cover((0..3).map(|id| (id, hidden)), PRIMARY, 2),
            StackCover::Unknown
        );
        assert!(first_cover((0..2).map(|id| (id, hidden)), PRIMARY, 2).is_clear());
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
                covered: true,
                ..fullscreen
            },
            true
        ));
        assert!(!accept_frame(Source::Display, fullscreen, windowed(), true));
        assert!(!accept_frame(Source::Window, windowed(), fullscreen, true));
    }
}
