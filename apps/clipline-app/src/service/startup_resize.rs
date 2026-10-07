//! Games such as Abiotic Factor and Ready or Not open fullscreen and then
//! resize to the player's window. The encoder size is fixed by the first
//! frame and later frames are stretched to fill it, so a resize early in a
//! recording means startup, not gameplay: restart at the new size.

use std::time::{Duration, Instant};

/// How long after a run's first frame a size change still counts as startup.
pub(super) const STARTUP_RESIZE_WINDOW: Duration = Duration::from_secs(10);
/// How long the new size must hold before restarting, so a game stepping
/// through several sizes restarts once.
pub(super) const STARTUP_RESIZE_SETTLE: Duration = Duration::from_secs(1);
/// Restarts per recorder start, so a game that keeps resizing can't loop.
pub(super) const MAX_STARTUP_RESTARTS: u32 = 3;

pub(super) struct StartupResizeWatch {
    encoded: (u32, u32),
    started: Instant,
    armed: bool,
    pending: Option<((u32, u32), Instant)>,
}

impl StartupResizeWatch {
    /// `encoded` is the captured size the encoder was built for; `armed` is
    /// false for sources that don't stretch or once restarts are used up.
    pub(super) fn new(encoded: (u32, u32), started: Instant, armed: bool) -> Self {
        Self {
            encoded,
            started,
            armed,
            pending: None,
        }
    }

    /// Feed each captured frame's size. True once the recording should
    /// restart at `size`.
    pub(super) fn observe(&mut self, size: (u32, u32), now: Instant) -> bool {
        if !self.armed || size == self.encoded {
            self.pending = None;
            return false;
        }
        match self.pending {
            Some((pending, since)) if pending == size => {
                now.duration_since(since) >= STARTUP_RESIZE_SETTLE
            }
            _ => {
                if now.duration_since(self.started) > STARTUP_RESIZE_WINDOW {
                    // Gameplay, not startup: keep today's behaviour.
                    self.armed = false;
                    return false;
                }
                self.pending = Some((size, now));
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULLSCREEN: (u32, u32) = (2560, 1440);
    const WINDOWED: (u32, u32) = (1600, 900);

    fn at(start: Instant, seconds: f64) -> Instant {
        start + Duration::from_secs_f64(seconds)
    }

    #[test]
    fn matching_frames_never_restart() {
        let start = Instant::now();
        let mut watch = StartupResizeWatch::new(FULLSCREEN, start, true);
        for second in 0..30 {
            assert!(!watch.observe(FULLSCREEN, at(start, second as f64)));
        }
    }

    #[test]
    fn restarts_once_a_startup_resize_has_settled() {
        let start = Instant::now();
        let mut watch = StartupResizeWatch::new(FULLSCREEN, start, true);
        assert!(!watch.observe(WINDOWED, at(start, 4.0)), "just changed");
        assert!(!watch.observe(WINDOWED, at(start, 4.5)), "still settling");
        assert!(watch.observe(WINDOWED, at(start, 5.0)), "held for the settle time");
    }

    #[test]
    fn a_resize_seen_just_before_the_window_closes_may_settle_after_it() {
        let start = Instant::now();
        let mut watch = StartupResizeWatch::new(FULLSCREEN, start, true);
        assert!(!watch.observe(WINDOWED, at(start, 9.8)));
        assert!(watch.observe(WINDOWED, at(start, 10.8)));
    }

    #[test]
    fn a_resize_after_startup_never_restarts() {
        let start = Instant::now();
        let mut watch = StartupResizeWatch::new(FULLSCREEN, start, true);
        assert!(!watch.observe(FULLSCREEN, at(start, 5.0)));
        for second in [10.5, 11.0, 12.0, 20.0] {
            assert!(!watch.observe(WINDOWED, at(start, second)), "{second}s");
        }
    }

    #[test]
    fn stepping_through_sizes_restarts_the_settle_timer() {
        let start = Instant::now();
        let mut watch = StartupResizeWatch::new(FULLSCREEN, start, true);
        assert!(!watch.observe((1920, 1080), at(start, 2.0)));
        assert!(!watch.observe(WINDOWED, at(start, 2.6)), "another size: wait again");
        assert!(!watch.observe(WINDOWED, at(start, 3.4)));
        assert!(watch.observe(WINDOWED, at(start, 3.6)));
    }

    #[test]
    fn returning_to_the_encoded_size_cancels() {
        let start = Instant::now();
        let mut watch = StartupResizeWatch::new(FULLSCREEN, start, true);
        assert!(!watch.observe(WINDOWED, at(start, 2.0)));
        assert!(!watch.observe(FULLSCREEN, at(start, 2.5)));
        assert!(!watch.observe(WINDOWED, at(start, 3.2)), "a new change starts over");
        assert!(watch.observe(WINDOWED, at(start, 4.2)));
    }

    #[test]
    fn a_disarmed_watch_never_restarts() {
        let start = Instant::now();
        let mut watch = StartupResizeWatch::new(FULLSCREEN, start, false);
        assert!(!watch.observe(WINDOWED, at(start, 1.0)));
        assert!(!watch.observe(WINDOWED, at(start, 5.0)));
    }
}
