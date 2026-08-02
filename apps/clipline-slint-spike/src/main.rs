use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    clipline_slint_spike::create_window()?.run()
}
