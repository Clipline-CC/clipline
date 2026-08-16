const OFFICIAL_BUG_REPORT_ENDPOINT: &str = "https://support.dain.cafe/api/v1/reports";

fn main() {
    println!("cargo:rerun-if-env-changed=CLIPLINE_BUG_REPORT_ENDPOINT");
    let configured_endpoint = std::env::var("CLIPLINE_BUG_REPORT_ENDPOINT")
        .unwrap_or_else(|_| OFFICIAL_BUG_REPORT_ENDPOINT.to_string());
    assert_eq!(
        configured_endpoint, OFFICIAL_BUG_REPORT_ENDPOINT,
        "CLIPLINE_BUG_REPORT_ENDPOINT must use Clipline's official private intake"
    );
    println!("cargo:rustc-env=CLIPLINE_BUG_REPORT_ENDPOINT={OFFICIAL_BUG_REPORT_ENDPOINT}");
    // Release workflows bake the channel the install package tracks so a
    // Stable download defaults to Stable updates and a Nightly download to
    // Nightly. Local dev builds keep the Nightly default.
    println!("cargo:rerun-if-env-changed=CLIPLINE_DEFAULT_UPDATE_CHANNEL");
    let default_channel = std::env::var("CLIPLINE_DEFAULT_UPDATE_CHANNEL")
        .unwrap_or_else(|_| "nightly".to_string());
    assert!(
        default_channel == "nightly" || default_channel == "stable",
        "CLIPLINE_DEFAULT_UPDATE_CHANNEL must be 'nightly' or 'stable', got '{default_channel}'"
    );
    println!("cargo:rustc-env=CLIPLINE_DEFAULT_UPDATE_CHANNEL={default_channel}");

    // The Tauri context only exists for Windows builds (see Cargo.toml).
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        tauri_build::build();
    }
}
