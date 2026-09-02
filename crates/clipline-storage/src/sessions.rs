//! Session folder naming and the run/match session state machine.
//!
//! A session is a folder under the clips root. Every recorder run gets one
//! (label fixed at service start); a detected match temporarily switches the
//! active session to its own folder and reverts when the match ends. Folder
//! creation is the caller's job, lazily at save time.

/// True for recorder-generated session folder names:
/// `2026-06-12 14-05` or `2026-06-12 14-52 league`.
///
/// Other media-root children (Screenshots, user folders) must not be treated
/// as emptied sessions.
pub fn is_session_dir_name(name: &str) -> bool {
    let stamp = match name.strip_suffix(" league") {
        Some(stamp) => stamp,
        None => name,
    };
    let Some((date, time)) = stamp.split_once(' ') else {
        return false;
    };
    let mut date_parts = date.split('-');
    let (Some(year), Some(month), Some(day), None) = (
        date_parts.next(),
        date_parts.next(),
        date_parts.next(),
        date_parts.next(),
    ) else {
        return false;
    };
    let mut time_parts = time.split('-');
    let (Some(hour), Some(minute), None) =
        (time_parts.next(), time_parts.next(), time_parts.next())
    else {
        return false;
    };
    year.len() == 4
        && month.len() == 2
        && day.len() == 2
        && hour.len() == 2
        && minute.len() == 2
        && year.bytes().all(|byte| byte.is_ascii_digit())
        && month.bytes().all(|byte| byte.is_ascii_digit())
        && day.bytes().all(|byte| byte.is_ascii_digit())
        && hour.bytes().all(|byte| byte.is_ascii_digit())
        && minute.bytes().all(|byte| byte.is_ascii_digit())
}

/// `2026-06-12 14-05` / `2026-06-12 14-52 league` — Windows-safe, sortable.
pub fn session_label(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    league_match: bool,
) -> String {
    let base = format!("{year:04}-{month:02}-{day:02} {hour:02}-{minute:02}");
    if league_match {
        format!("{base} league")
    } else {
        base
    }
}

/// Tracks which session folder saves should land in right now.
#[derive(Debug, Clone)]
pub struct SessionTracker {
    run_label: String,
    match_label: Option<String>,
}

impl SessionTracker {
    pub fn new(run_label: String) -> Self {
        Self {
            run_label,
            match_label: None,
        }
    }

    pub fn match_started(&mut self, label: String) {
        self.match_label = Some(label);
    }

    pub fn match_ended(&mut self) {
        self.match_label = None;
    }

    pub fn current(&self) -> &str {
        self.match_label.as_deref().unwrap_or(&self.run_label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_zero_padded_and_windows_safe() {
        assert_eq!(session_label(2026, 6, 12, 14, 5, false), "2026-06-12 14-05");
        assert_eq!(
            session_label(2026, 6, 12, 14, 52, true),
            "2026-06-12 14-52 league"
        );
        assert!(is_session_dir_name("2026-06-12 14-05"));
        assert!(is_session_dir_name("2026-06-12 14-52 league"));
        assert!(!is_session_dir_name("Screenshots"));
        assert!(!is_session_dir_name("2026-06-12"));
        assert!(!is_session_dir_name("ranked-match"));
    }

    #[test]
    fn tracker_switches_to_match_and_reverts() {
        let mut tracker = SessionTracker::new("2026-06-12 14-30".into());
        assert_eq!(tracker.current(), "2026-06-12 14-30");

        tracker.match_started("2026-06-12 14-52 league".into());
        assert_eq!(tracker.current(), "2026-06-12 14-52 league");

        tracker.match_ended();
        assert_eq!(tracker.current(), "2026-06-12 14-30");

        // A second end is harmless.
        tracker.match_ended();
        assert_eq!(tracker.current(), "2026-06-12 14-30");
    }
}
