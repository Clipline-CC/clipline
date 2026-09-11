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

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InitialCanvas {
    pub size: (u32, u32),
    pub reason: &'static str,
}

/// Reserve monitor-relative headroom without imposing the monitor's aspect.
/// Dimensions are validated capture sizes, not window-frame bounds. A fixed
/// canvas can upscale smaller inputs and letterbox later aspect changes.
pub(crate) fn initial_canvas(client: Option<(u32, u32)>, display: (u32, u32)) -> InitialCanvas {
    let fallback = |reason| InitialCanvas {
        size: display,
        reason,
    };
    let Some((width, height)) = client else {
        return fallback("client_unavailable");
    };
    // Avoid startup placeholders and aspect fits too thin for useful encoding.
    const MIN_SIDE: u32 = 64;
    if width < MIN_SIDE || height < MIN_SIDE {
        return fallback("client_too_small");
    }
    let fitted =
        crate::video_layout::fitted_video_rect(width, height, display.0 & !1, display.1 & !1);
    match fitted {
        Ok(rect) if rect.width >= MIN_SIDE && rect.height >= MIN_SIDE => InitialCanvas {
            size: (rect.width, rect.height),
            reason: "client_aspect_monitor_budget",
        },
        _ => fallback("client_aspect_unusable"),
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
    fn window_canvas_uses_monitor_headroom_with_game_aspect() {
        let canvas = initial_canvas(Some((1920, 1080)), (5120, 1440)).size;
        assert_eq!(canvas, (2560, 1440));
        assert_eq!(canvas.0 * 9, canvas.1 * 16);
        assert_eq!(initial_canvas(Some((1280, 720)), (5120, 1440)).size, canvas);
        assert_eq!(
            initial_canvas(Some((2560, 1440)), (5120, 1440)).size,
            canvas
        );
        let fullscreen =
            crate::video_layout::fitted_video_rect(5120, 1440, canvas.0, canvas.1).unwrap();
        assert_eq!((fullscreen.width, fullscreen.height), (2560, 720));
        assert_eq!((fullscreen.x, fullscreen.y), (0, 360));
    }

    #[test]
    fn canvas_falls_back_for_tiny_or_extreme_startup_clients() {
        for (size, reason) in [
            ((1, 1), "client_too_small"),
            ((63, 720), "client_too_small"),
            ((1280, 63), "client_too_small"),
            ((64, 10000), "client_aspect_unusable"),
        ] {
            let canvas = initial_canvas(Some(size), (5120, 1440));
            assert_eq!(canvas.size, (5120, 1440));
            assert_eq!(canvas.reason, reason);
        }
    }

    #[test]
    fn canvas_fitting_handles_odd_display_bounds_and_portrait_aspect() {
        assert_eq!(
            initial_canvas(Some((1920, 1080)), (5119, 1439)).size,
            (2556, 1438)
        );
        assert_eq!(
            initial_canvas(Some((900, 1600)), (2560, 1440)).size,
            (810, 1440)
        );
    }

    #[test]
    fn full_display_client_keeps_ultrawide_canvas() {
        let canvas = initial_canvas(Some((5120, 1440)), (5120, 1440));
        assert_eq!(canvas.size, (5120, 1440));
        assert_eq!(canvas.reason, "client_aspect_monitor_budget");
    }

    #[test]
    fn unavailable_initial_client_keeps_existing_display_seed() {
        let canvas = initial_canvas(None, (5120, 1440));
        assert_eq!(canvas.size, (5120, 1440));
        assert_eq!(canvas.reason, "client_unavailable");
    }

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
