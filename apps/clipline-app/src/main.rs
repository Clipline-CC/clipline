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
    if let Err(error) = windows_main() {
        eprintln!("Clipline startup failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn windows_main() -> Result<(), String> {
    if print_benchmark_probe_if_requested() {
        return Ok(());
    }
    let launch = clipline_shell::ShellLaunch::parse(std::env::args())
        .map_err(|error| format!("parse shell launch: {error}"))?;
    if let Err(error) =
        clipline_shell::windows::process::wait_for_elevation_parent(launch.elevation_parent())
    {
        return Err(format!("administrator restart handoff: {error}"));
    }
    if let Some(parent) = launch.updater_parent() {
        if let Err(error) = clipline_shell::windows::process::wait_for_process_exit(parent) {
            return Err(format!("update restart handoff: {error}"));
        }
    }
    let command = match launch.mode() {
        clipline_shell::LaunchMode::Autostart => {
            clipline_shell::activation::ActivationCommand::AutostartNoop
        }
        clipline_shell::LaunchMode::Normal => clipline_shell::activation::ActivationCommand::Reveal,
    };
    let (shell_sender, shell_receiver) = clipline_shell::shell_command_channel();
    match clipline_shell::windows::activation::acquire_or_activate(
        "io.clipline.app",
        command,
        shell_sender.clone(),
    )
    .map_err(|error| format!("single-instance activation: {error}"))?
    {
        clipline_shell::windows::activation::WindowsInstanceRole::Primary(instance) => {
            app::run(instance, shell_sender, shell_receiver, launch);
        }
        clipline_shell::windows::activation::WindowsInstanceRole::Secondary(_) => {}
    }
    Ok(())
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
mod library;
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
mod settings_probe;
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
