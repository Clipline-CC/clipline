//! Non-distributed Slint presentation spike for Clipline.

slint::include_modules!();

pub fn create_window() -> Result<CliplineSpike, slint::PlatformError> {
    CliplineSpike::new()
}
