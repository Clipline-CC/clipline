//! Neutral source policy for windowed WGC / fullscreen Desktop Duplication.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Window,
    Display,
    Waiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Observation {
    pub available: bool,
    pub foreground: bool,
    pub covers_monitor: bool,
}

pub(crate) fn choose(observation: Observation) -> Source {
    if !observation.available {
        Source::Waiting
    } else if !observation.covers_monitor {
        Source::Window
    } else if observation.foreground {
        Source::Display
    } else {
        Source::Waiting
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
            foreground: true,
            covers_monitor: false,
        }
    }

    #[test]
    fn ordinary_windows_stay_on_wgc_even_in_the_background() {
        assert_eq!(choose(windowed()), Source::Window);
        assert_eq!(
            choose(Observation {
                foreground: false,
                ..windowed()
            }),
            Source::Window
        );
    }

    #[test]
    fn fullscreen_switch_requires_the_exact_foreground_window() {
        let fullscreen = Observation {
            covers_monitor: true,
            ..windowed()
        };
        assert_eq!(choose(fullscreen), Source::Display);
        assert_eq!(
            choose(Observation {
                foreground: false,
                ..fullscreen
            }),
            Source::Waiting
        );
        assert_eq!(
            choose(Observation {
                available: false,
                ..fullscreen
            }),
            Source::Waiting
        );
    }

    #[test]
    fn frames_are_discarded_when_a_guard_or_monitor_changes() {
        let fullscreen = Observation {
            covers_monitor: true,
            ..windowed()
        };
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
                foreground: false,
                ..fullscreen
            },
            true
        ));
        assert!(!accept_frame(Source::Display, fullscreen, windowed(), true));
        assert!(!accept_frame(Source::Window, windowed(), fullscreen, true));
    }
}
