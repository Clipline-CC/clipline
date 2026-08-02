fn main() {
    println!("cargo:rerun-if-env-changed=OPT_LEVEL");
    let opt_level = std::env::var("OPT_LEVEL").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=CLIPLINE_BUILD_OPT_LEVEL={opt_level}");

    let regular = std::env::var_os("CARGO_FEATURE_PACKAGE_REGULAR").is_some();
    let standalone = std::env::var_os("CARGO_FEATURE_PACKAGE_STANDALONE").is_some();
    assert!(
        !(regular && standalone),
        "package-regular and package-standalone are mutually exclusive"
    );
    let variant = if regular {
        "regular"
    } else if standalone {
        "standalone"
    } else {
        "unpackaged"
    };
    println!("cargo:rustc-env=CLIPLINE_PACKAGE_VARIANT={variant}");

    let product_config_path = "../clipline-app/tauri.conf.json";
    println!("cargo:rerun-if-changed={product_config_path}");
    let product_config: serde_json::Value = serde_json::from_slice(
        &std::fs::read(product_config_path).expect("read Clipline product configuration"),
    )
    .expect("parse Clipline product configuration");
    for (field, expected) in [
        ("productName", "Clipline"),
        ("identifier", "io.clipline.app"),
    ] {
        assert_eq!(
            product_config[field].as_str(),
            Some(expected),
            "Slint package metadata must preserve {field}"
        );
    }
    let version = product_config["version"]
        .as_str()
        .expect("Clipline product version must be a string");
    assert!(
        version.split('.').all(|component| !component.is_empty()
            && component.chars().all(|value| value.is_ascii_digit()))
            && version.split('.').count() == 3,
        "Slint package version must be a stable three-component version"
    );
    println!("cargo:rustc-env=CLIPLINE_PACKAGE_VERSION={version}");
    slint_build::compile("ui/app.slint").expect("compile Slint presentation spike UI");
}
