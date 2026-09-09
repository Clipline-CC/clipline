//! Conservative policy for the explicitly selected experimental hybrid.
//! Shell fullscreen state is a global heuristic, not presentation telemetry.

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
    pub single_monitor: bool,
    /// None includes locked/session-unavailable and failed shell queries.
    pub fullscreen: Option<bool>,
}

pub(crate) fn choose(o: Observation) -> Source {
    if !o.available {
        return Source::Waiting;
    }
    match o.fullscreen {
        Some(false) => Source::Window,
        Some(true) if o.foreground && o.covers_monitor && o.single_monitor => Source::Display,
        _ => Source::Waiting,
    }
}

pub(crate) fn accept_frame(
    source: Source,
    before: Observation,
    after: Observation,
    same_surface: bool,
) -> bool {
    source != Source::Waiting
        && same_surface
        && before == after
        && choose(before) == source
        && choose(after) == source
}

pub(crate) fn accept_window_packet(
    request: Observation,
    current: Observation,
    same_surface: bool,
    age: std::time::Duration,
) -> bool {
    accept_frame(Source::Window, request, current, same_surface)
        && age <= std::time::Duration::from_millis(250)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_window_packet_keeps_its_request_context_and_has_a_freshness_deadline() {
        use std::time::Duration;
        let request = visible();
        assert!(accept_window_packet(
            request,
            request,
            true,
            Duration::from_millis(30)
        ));
        assert!(!accept_window_packet(
            request,
            request,
            true,
            Duration::from_millis(251)
        ));
        assert!(!accept_window_packet(
            request,
            request,
            false,
            Duration::ZERO
        ));
        for current in [
            Observation {
                foreground: false,
                ..request
            },
            Observation {
                fullscreen: Some(true),
                ..request
            },
            Observation {
                available: false,
                ..request
            },
        ] {
            assert!(!accept_window_packet(
                request,
                current,
                true,
                Duration::ZERO
            ));
        }
    }

    fn visible() -> Observation {
        Observation {
            available: true,
            foreground: true,
            covers_monitor: true,
            single_monitor: true,
            fullscreen: Some(false),
        }
    }

    #[test]
    fn monitor_sized_borderless_and_overlapped_windows_stay_isolated() {
        assert_eq!(choose(visible()), Source::Window);
        assert_eq!(
            choose(Observation {
                foreground: false,
                ..visible()
            }),
            Source::Window
        );
    }

    #[test]
    fn display_requires_every_guard_and_positive_fullscreen_signal() {
        let full = Observation {
            fullscreen: Some(true),
            ..visible()
        };
        assert_eq!(choose(full), Source::Display);
        for o in [
            Observation {
                available: false,
                ..full
            },
            Observation {
                foreground: false,
                ..full
            },
            Observation {
                covers_monitor: false,
                ..full
            },
            Observation {
                single_monitor: false,
                ..full
            },
            Observation {
                fullscreen: None,
                ..full
            },
        ] {
            assert_eq!(choose(o), Source::Waiting);
        }
    }

    #[test]
    fn unavailable_and_unknown_never_open_a_capture_source() {
        assert_eq!(
            choose(Observation {
                available: false,
                ..visible()
            }),
            Source::Waiting
        );
        assert_eq!(
            choose(Observation {
                fullscreen: None,
                ..visible()
            }),
            Source::Waiting
        );
    }

    #[test]
    fn topology_loss_rejects_both_sources_until_a_new_valid_observation() {
        for fullscreen in [false, true] {
            let valid = Observation {
                fullscreen: Some(fullscreen),
                ..visible()
            };
            let lost = Observation {
                available: false,
                covers_monitor: false,
                single_monitor: false,
                ..valid
            };
            let source = choose(valid);
            assert_eq!(choose(lost), Source::Waiting);
            assert!(!accept_frame(source, valid, lost, true));
            assert!(!accept_frame(source, lost, valid, true));
            assert!(!accept_window_packet(
                valid,
                lost,
                true,
                std::time::Duration::ZERO
            ));
            assert!(accept_frame(source, valid, valid, true));
        }
    }

    #[test]
    fn discard_acquired_display_frame_on_any_post_capture_guard_change() {
        let before = Observation {
            fullscreen: Some(true),
            ..visible()
        };
        assert!(accept_frame(Source::Display, before, before, true));
        for after in [
            Observation {
                foreground: false,
                ..before
            },
            Observation {
                available: false,
                ..before
            },
            Observation {
                fullscreen: Some(false),
                ..before
            },
            Observation {
                single_monitor: false,
                ..before
            },
        ] {
            assert!(!accept_frame(Source::Display, before, after, true));
        }
        assert!(!accept_frame(Source::Display, before, before, false));
    }
}
