#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    if print_benchmark_probe_if_requested() {
        return;
    }
    eprintln!("clipline-app is Windows-only (capture/encode are platform-bound)");
}

#[cfg(windows)]
fn main() {
    if print_benchmark_probe_if_requested() {
        return;
    }
    if let Err(error) = windows::wait_for_elevation_parent_from_args() {
        eprintln!("administrator restart handoff: {error}");
        return;
    }
    app::run();
}

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod bounded_http;
#[cfg(windows)]
mod cloud;
#[cfg(windows)]
mod cloud_upload;
#[cfg(windows)]
mod credential_transaction;
#[cfg(windows)]
mod desktop;
#[cfg(windows)]
mod game_discovery;
#[cfg(windows)]
mod game_icon;
#[cfg(windows)]
mod game_identity;
#[cfg(windows)]
mod game_plugins;
#[cfg(windows)]
mod games;
#[cfg(windows)]
mod hotkeys;
#[cfg(windows)]
mod library;
#[cfg(windows)]
mod markers;
#[cfg(windows)]
mod memory;
#[cfg(windows)]
mod osu_api;
#[cfg(windows)]
mod osu_enrichment;
#[cfg(windows)]
mod poster;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod settings;
#[cfg(windows)]
mod sound;
#[cfg(windows)]
mod updates;
#[cfg(windows)]
mod util;
#[cfg(windows)]
mod windows;

const BENCHMARK_PROBE_ARGUMENT: &str = "--clipline-benchmark-probe";

fn benchmark_shell_is_safe(debug_assertions: bool, opt_level: &str) -> bool {
    debug_assertions && !matches!(opt_level.trim(), "" | "0" | "unknown")
}

fn print_benchmark_probe_if_requested() -> bool {
    if !std::env::args().any(|argument| argument == BENCHMARK_PROBE_ARGUMENT) {
        return false;
    }
    let opt_level = env!("CLIPLINE_BUILD_OPT_LEVEL");
    let safe = benchmark_shell_is_safe(cfg!(debug_assertions), opt_level);
    println!(
        concat!(
            "{{\"schema\":1,\"benchmark_shell_safe\":{},",
            "\"debug_assertions\":{},\"opt_level\":\"{}\",",
            "\"autostart_registry_mutation\":{}}}"
        ),
        safe,
        cfg!(debug_assertions),
        opt_level,
        !cfg!(debug_assertions)
    );
    true
}

#[cfg(test)]
mod benchmark_probe_tests {
    use super::benchmark_shell_is_safe;

    #[test]
    fn benchmark_probe_requires_optimization_and_registry_safe_assertions() {
        assert!(benchmark_shell_is_safe(true, "3"));
        assert!(benchmark_shell_is_safe(true, "s"));
        assert!(!benchmark_shell_is_safe(true, "0"));
        assert!(!benchmark_shell_is_safe(false, "3"));
    }
}
