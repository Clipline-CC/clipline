#[cfg(windows)]
use std::cell::RefCell;
#[cfg(windows)]
use std::rc::Rc;
#[cfg(windows)]
use std::time::Duration;

use clipline_slint_spike::options::{OptionsError, SpikeOptions};
use slint::ComponentHandle;

#[cfg(windows)]
struct RuntimeState {
    session: Option<clipline_slint_spike::live::LiveSession>,
    host: Option<clipline_playback::windows::WindowsVideoHost>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = match SpikeOptions::parse(std::env::args_os()) {
        Ok(options) => options,
        Err(OptionsError::HelpRequested) => {
            println!("{}", SpikeOptions::usage());
            return Ok(());
        }
        Err(error) => {
            eprintln!("{error}\n{}", SpikeOptions::usage());
            return Err(Box::new(error));
        }
    };
    std::env::set_var("SLINT_BACKEND", &options.renderer);
    run(options)
}

#[cfg(not(windows))]
fn run(options: SpikeOptions) -> Result<(), Box<dyn std::error::Error>> {
    if options.fixture.is_some() {
        return Err("native playback is currently implemented for Windows".into());
    }
    clipline_slint_spike::create_window()?.run()?;
    Ok(())
}

#[cfg(windows)]
fn run(options: SpikeOptions) -> Result<(), Box<dyn std::error::Error>> {
    use clipline_slint_spike::controller::ShutdownOrder;
    use clipline_slint_spike::windows::{occlude_video_host, update_video_host};
    use clipline_slint_spike::SpikeTray;

    let window = clipline_slint_spike::create_window()?;
    window.set_cpu_frame_diagnostic(options.cpu_frame_diagnostic);
    let tray = SpikeTray::new()?;
    tray.set_tray_icon(tray_icon());
    let runtime = Rc::new(RefCell::new(RuntimeState {
        session: None,
        host: None,
    }));

    {
        let runtime = Rc::clone(&runtime);
        let weak = window.as_weak();
        window.on_play_pause(move || {
            with_controller(&runtime, &weak, |controller| controller.play_pause());
        });
    }
    {
        let runtime = Rc::clone(&runtime);
        let weak = window.as_weak();
        window.on_seek(move |seconds| {
            with_controller(&runtime, &weak, |controller| {
                controller.seek_relative(f64::from(seconds))
            });
        });
    }
    {
        let runtime = Rc::clone(&runtime);
        let weak = window.as_weak();
        window.on_set_track(move |track, selected| {
            let Ok(track) = usize::try_from(track) else {
                return;
            };
            with_controller(&runtime, &weak, |controller| {
                controller.set_track(track, selected)
            });
        });
    }
    {
        let runtime = Rc::clone(&runtime);
        let weak = window.as_weak();
        window.on_set_volume(move |volume| {
            with_controller(&runtime, &weak, |controller| controller.set_volume(volume));
        });
    }
    {
        let runtime = Rc::clone(&runtime);
        let weak = window.as_weak();
        window.on_video_geometry_changed(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if let Some(host) = runtime.borrow_mut().host.as_mut() {
                if let Err(error) = update_video_host(host, &window) {
                    window.set_status_text(error.into());
                }
            }
        });
    }
    window.on_show_library(|| {});
    window.on_show_review(|| {});

    {
        let runtime = Rc::clone(&runtime);
        window.window().on_close_requested(move || {
            let mut runtime = runtime.borrow_mut();
            if let Some(session) = runtime.session.as_ref() {
                if let Ok(controller) = session.controller().lock() {
                    let _ = controller.pause();
                }
            }
            if let Some(host) = runtime.host.as_mut() {
                let _ = occlude_video_host(host);
            }
            slint::CloseRequestResponse::HideWindow
        });
    }
    {
        let weak = window.as_weak();
        let runtime = Rc::clone(&runtime);
        tray.on_show_window(move || {
            if let Some(window) = weak.upgrade() {
                let _ = window.show();
                let weak = window.as_weak();
                let runtime = Rc::clone(&runtime);
                slint::Timer::single_shot(Duration::ZERO, move || {
                    if let Some(window) = weak.upgrade() {
                        if let Some(host) = runtime.borrow_mut().host.as_mut() {
                            let _ = update_video_host(host, &window);
                        }
                    }
                });
            }
        });
    }
    tray.on_quit_app(|| {
        let _ = slint::quit_event_loop();
    });

    window.show()?;
    tray.show()?;
    schedule_live_start(window.as_weak(), Rc::clone(&runtime), options, 0);

    slint::run_event_loop()?;

    let mut shutdown_order = ShutdownOrder::default();
    let (session, mut host) = {
        let mut runtime = runtime.borrow_mut();
        (runtime.session.take(), runtime.host.take())
    };
    if let Some(session) = session {
        let report = session.shutdown().map_err(std::io::Error::other)?;
        if let Some(telemetry) = report.telemetry.as_ref() {
            eprintln!(
                "Slint native session: exit={:?} presentation={:?} decoded={} presented={} late_or_dropped={} mft={}/{} midstream_underruns={}",
                report.exit,
                report.presentation,
                telemetry.metrics.decoded_eligible_frames,
                telemetry.metrics.presented_frames,
                telemetry.metrics.late_or_dropped_frames,
                telemetry.decoder_ownership.mft_samples_received,
                telemetry.decoder_ownership.mft_samples_released,
                telemetry.audio_midstream_underruns,
            );
        } else {
            eprintln!(
                "Slint native session: exit={:?} presentation={:?} no-media",
                report.exit, report.presentation
            );
        }
    }
    shutdown_order.session_stopped()?;
    if let Some(host) = host.as_mut() {
        host.close().map_err(std::io::Error::other)?;
    }
    drop(host);
    shutdown_order.host_destroyed()?;
    drop(tray);
    drop(window);
    shutdown_order.ui_dropped()?;
    Ok(())
}

#[cfg(windows)]
fn schedule_live_start(
    weak: slint::Weak<clipline_slint_spike::CliplineSpike>,
    runtime: Rc<RefCell<RuntimeState>>,
    options: SpikeOptions,
    attempt: u16,
) {
    const MAX_HANDLE_ATTEMPTS: u16 = 200;
    slint::Timer::single_shot(Duration::from_millis(10), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let Some(fixture) = options.fixture.clone() else {
            window.set_status_text("Static Slint shell ready · no fixture selected".into());
            if let Some(path) = options.marker_path.as_ref() {
                let _ = clipline_slint_spike::options::write_marker(
                    path,
                    "ready",
                    "interactive static Slint shell ready",
                );
            }
            return;
        };
        window.set_review_visible(true);
        let (publisher, host) = if options.cpu_frame_diagnostic {
            (
                clipline_slint_spike::live::SpikePublisher::Cpu(
                    clipline_slint_spike::cpu_frame::CpuDiagnosticPublisher::new(window.as_weak()),
                ),
                None,
            )
        } else {
            let (mut host, target) =
                match clipline_slint_spike::windows::attach_video_host(window.window()) {
                    Ok(value) => value,
                    Err(_) if attempt + 1 < MAX_HANDLE_ATTEMPTS => {
                        window.set_status_text("Waiting for Slint Win32 handle".into());
                        schedule_live_start(weak, runtime, options, attempt + 1);
                        return;
                    }
                    Err(error) => {
                        report_start_error(&window, options.marker_path.as_ref(), error);
                        return;
                    }
                };
            if let Err(error) = clipline_slint_spike::windows::update_video_host(&mut host, &window)
            {
                report_start_error(&window, options.marker_path.as_ref(), error);
                return;
            }
            (
                clipline_slint_spike::live::SpikePublisher::D3d(
                    clipline_playback::windows::WindowsD3D11Publisher::new(target),
                ),
                Some(host),
            )
        };
        let session = match clipline_slint_spike::live::LiveSession::start(
            publisher,
            window.as_weak(),
            fixture,
            options.scenario,
            options.marker_path.clone(),
            options.exit_after_ready,
        ) {
            Ok(session) => session,
            Err(error) => {
                report_start_error(&window, options.marker_path.as_ref(), error);
                return;
            }
        };
        {
            let mut runtime_state = runtime.borrow_mut();
            runtime_state.host = host;
            runtime_state.session = Some(session);
        }
        if options.scenario == clipline_slint_spike::options::SpikeScenario::RevealClose100 {
            schedule_reveal_close_cycle(
                window.as_weak(),
                runtime,
                options.marker_path.clone(),
                options.exit_after_ready,
                0,
                true,
            );
        }
    });
}

#[cfg(windows)]
fn schedule_reveal_close_cycle(
    weak: slint::Weak<clipline_slint_spike::CliplineSpike>,
    runtime: Rc<RefCell<RuntimeState>>,
    marker_path: Option<std::path::PathBuf>,
    exit_after_ready: bool,
    cycle: u16,
    hide_phase: bool,
) {
    const REQUIRED_CYCLES: u16 = 100;
    if cycle == REQUIRED_CYCLES {
        if let Some(path) = marker_path.as_ref() {
            let _ = clipline_slint_spike::options::write_marker(
                path,
                "ready",
                "reveal-close-100 completed 100 Slint hide/reveal cycles",
            );
        }
        if exit_after_ready {
            let _ = slint::quit_event_loop();
        }
        return;
    }
    slint::Timer::single_shot(Duration::from_millis(10), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if hide_phase {
            if let Some(host) = runtime.borrow_mut().host.as_mut() {
                let _ = clipline_slint_spike::windows::occlude_video_host(host);
            }
            let _ = window.hide();
            schedule_reveal_close_cycle(weak, runtime, marker_path, exit_after_ready, cycle, false);
        } else {
            let _ = window.show();
            if let Some(host) = runtime.borrow_mut().host.as_mut() {
                let _ = clipline_slint_spike::windows::update_video_host(host, &window);
            }
            schedule_reveal_close_cycle(
                weak,
                runtime,
                marker_path,
                exit_after_ready,
                cycle + 1,
                true,
            );
        }
    });
}

#[cfg(windows)]
fn with_controller(
    runtime: &Rc<RefCell<RuntimeState>>,
    window: &slint::Weak<clipline_slint_spike::CliplineSpike>,
    command: impl FnOnce(
        &clipline_slint_spike::controller::PlaybackController<
            clipline_slint_spike::live::SessionCommandPort,
        >,
    ) -> Result<(), clipline_slint_spike::controller::ControllerError>,
) {
    let result = runtime
        .borrow()
        .session
        .as_ref()
        .and_then(|session| {
            session
                .controller()
                .lock()
                .ok()
                .map(|controller| command(&controller))
        })
        .unwrap_or(Ok(()));
    if let Err(error) = result {
        if let Some(window) = window.upgrade() {
            window.set_status_text(format!("Controller error: {error}").into());
        }
    }
}

#[cfg(windows)]
fn report_start_error(
    window: &clipline_slint_spike::CliplineSpike,
    marker_path: Option<&std::path::PathBuf>,
    error: String,
) {
    window.set_status_text(format!("Native spike failed: {error}").into());
    if let Some(path) = marker_path {
        let _ = clipline_slint_spike::options::write_marker(path, "error", &error);
    }
}

#[cfg(windows)]
fn tray_icon() -> slint::Image {
    let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(16, 16);
    for (index, pixel) in pixels.make_mut_slice().iter_mut().enumerate() {
        let x = index % 16;
        let y = index / 16;
        *pixel = if (x as isize - 8).pow(2) + (y as isize - 8).pow(2) <= 49 {
            slint::Rgba8Pixel::new(217, 150, 42, 255)
        } else {
            slint::Rgba8Pixel::new(0, 0, 0, 0)
        };
    }
    slint::Image::from_rgba8(pixels)
}
