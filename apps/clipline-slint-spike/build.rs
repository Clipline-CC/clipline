fn main() {
    println!("cargo:rerun-if-env-changed=OPT_LEVEL");
    let opt_level = std::env::var("OPT_LEVEL").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=CLIPLINE_BUILD_OPT_LEVEL={opt_level}");
    slint_build::compile("ui/app.slint").expect("compile Slint presentation spike UI");
}
