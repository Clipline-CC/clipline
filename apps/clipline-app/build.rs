const OFFICIAL_BUG_REPORT_ENDPOINT: &str = "https://support.dain.cafe/api/v1/reports";

fn main() {
    println!("cargo:rerun-if-env-changed=OPT_LEVEL");
    let opt_level = std::env::var("OPT_LEVEL").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=CLIPLINE_BUILD_OPT_LEVEL={opt_level}");

    println!("cargo:rerun-if-env-changed=CLIPLINE_BUG_REPORT_ENDPOINT");
    let configured_endpoint = std::env::var("CLIPLINE_BUG_REPORT_ENDPOINT")
        .unwrap_or_else(|_| OFFICIAL_BUG_REPORT_ENDPOINT.to_string());
    assert_eq!(
        configured_endpoint, OFFICIAL_BUG_REPORT_ENDPOINT,
        "CLIPLINE_BUG_REPORT_ENDPOINT must use Clipline's official private intake"
    );
    println!("cargo:rustc-env=CLIPLINE_BUG_REPORT_ENDPOINT={OFFICIAL_BUG_REPORT_ENDPOINT}");

    // The Tauri context only exists for Windows builds (see Cargo.toml).
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        tauri_build::build();
    }
}
