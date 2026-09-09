//! Standalone experiment; never selected by Clipline's production recorder.
#[cfg(windows)]
#[path = "windows/print_window_record.rs"]
mod platform;

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    platform::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("print_window_record is Windows-only");
}

// Neutral protocol validation is exercised on both CI platforms.
#[cfg(windows)]
use clipline_capture::print_protocol as protocol;
