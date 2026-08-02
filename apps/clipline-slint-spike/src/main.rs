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
    if print_benchmark_probe_if_requested() {
        return Ok(());
    }
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
            "\"autostart_registry_mutation\":false}}"
        ),
        safe,
        cfg!(debug_assertions),
        opt_level,
    );
    true
}

#[cfg(not(windows))]
fn run(options: SpikeOptions) -> Result<(), Box<dyn std::error::Error>> {
    if options.fixture.is_some() {
        return Err("native playback is currently implemented for Windows".into());
    }
    let window = clipline_slint_spike::create_window()?;
    let desktop_adapter = start_desktop_adapter(&window)?;
    window.run()?;
    drop(desktop_adapter);
    Ok(())
}

#[cfg(windows)]
fn run(options: SpikeOptions) -> Result<(), Box<dyn std::error::Error>> {
    use clipline_slint_spike::controller::ShutdownOrder;
    use clipline_slint_spike::windows::{occlude_video_host, update_video_host};
    use clipline_slint_spike::SpikeTray;

    let window = clipline_slint_spike::create_window()?;
    let desktop_adapter = start_desktop_adapter(&window)?;
    let telemetry_path = options.telemetry_path.clone();
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
        if let Some(path) = telemetry_path.as_ref() {
            write_run_telemetry(path, &report)?;
        }
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
    drop(desktop_adapter);
    drop(window);
    shutdown_order.ui_dropped()?;
    Ok(())
}

fn start_desktop_adapter(
    window: &clipline_slint_spike::CliplineSpike,
) -> Result<clipline_slint_spike::desktop::SlintDesktopAdapter, std::io::Error> {
    let adapter = clipline_slint_spike::desktop::SlintDesktopAdapter::start(window.as_weak())
        .map_err(std::io::Error::other)?;
    adapter
        .try_publish(clipline_desktop::UiEvent::Recorder {
            generation: clipline_desktop::Generation::new(1),
            event: clipline_desktop::RecorderEvent::Status {
                recording: true,
                waiting_for_game: false,
                segments: 2,
                buffered_s: 30.0,
                buffered_mb: 24.0,
                full_session: false,
                encoder: "H.264".into(),
                capture_backend: "windows_graphics_capture".into(),
            },
        })
        .map_err(std::io::Error::other)?;
    Ok(adapter)
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
            options.stop_path.clone(),
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

#[cfg(windows)]
fn write_run_telemetry(
    path: &std::path::Path,
    report: &clipline_slint_spike::live::LiveSessionReport,
) -> Result<(), Box<dyn std::error::Error>> {
    use clipline_slint_spike::live::PresentationTelemetry;

    let presentation = match report.presentation {
        PresentationTelemetry::D3d(telemetry) => serde_json::json!({
            "path": "d3d11-child-window",
            "swapChainBuffers": clipline_playback::windows::PRESENTATION_SWAP_CHAIN_BUFFERS,
            "swapChainCreations": telemetry.swap_chain_creations,
            "swapChainResizes": telemetry.swap_chain_resizes,
            "presentedFrames": telemetry.presented_frames,
            "backpressuredFrames": telemetry.backpressured_frames,
            "occludedFrames": telemetry.occluded_frames,
            "deviceLosses": telemetry.device_losses,
            "adapterLuid": telemetry.adapter_luid,
        }),
        PresentationTelemetry::Cpu {
            frames,
            readback_frames,
            max_copy_time_100ns,
        } => serde_json::json!({
            "path": "cpu-shared-pixel-buffer-diagnostic",
            "rgbCapacity": frames.rgb_capacity,
            "allocationCount": frames.allocation_count,
            "replacedFrames": frames.replaced_frames,
            "staleFrames": frames.stale_frames,
            "backpressuredFrames": frames.backpressured_frames,
            "pendingHighWater": frames.pending_high_water,
            "readbackFrames": readback_frames,
            "maxCopyTime100ns": max_copy_time_100ns,
        }),
    };
    let session = report.telemetry.as_ref();
    let decoder = session
        .and_then(|telemetry| telemetry.decoder_info)
        .map(|decoder| {
            serde_json::json!({
                "acceleration": format!("{:?}", decoder.acceleration).to_lowercase(),
                "pixelFormat": format!("{:?}", decoder.pixel_format).to_lowercase(),
                "width": decoder.width,
                "height": decoder.height,
                "adapterLuid": decoder.adapter_luid,
            })
        });
    let value = serde_json::json!({
        "schemaVersion": 1,
        "slint": {
            "version": "1.17.1",
            "features": ["accessibility", "backend-winit", "compat-1-2", "raw-window-handle-06", "renderer-software", "std", "system-tray"],
        },
        "renderer": "winit-software",
        "sessionExit": format!("{:?}", report.exit).to_lowercase(),
        "bounds": {
            "commandInboxCapacity": clipline_playback::COMMAND_INBOX_CAPACITY,
            "sessionUpdateCapacity": clipline_playback::windows::SESSION_UPDATE_CAPACITY,
            "decoderSurfacePool": clipline_playback::windows::MAX_PLAYBACK_SURFACES,
            "presentationInputSurfaces": clipline_playback::windows::MAX_PRESENTATION_INPUT_SURFACES,
            "swapChainBuffers": clipline_playback::windows::PRESENTATION_SWAP_CHAIN_BUFFERS,
            "audioQueueFrames": clipline_playback::MAX_AUDIO_QUEUE_FRAMES,
            "audioWriteFrames": clipline_playback::MAX_AUDIO_WRITE_FRAMES,
            "opusPacketBytes": clipline_playback::MAX_OPUS_PACKET_BYTES,
            "encodedVideoSampleBytes": clipline_playback::MAX_ENCODED_VIDEO_SAMPLE_BYTES,
            "annexBAccessUnitBytes": clipline_playback::MAX_ANNEX_B_ACCESS_UNIT_BYTES,
            "diagnosticRgbPixels": clipline_playback::windows::MAX_DIAGNOSTIC_RGB_PIXELS,
        },
        "presentation": presentation,
        "decoder": decoder,
        "audioEndpoint": session.map(|telemetry| serde_json::json!({
            "id": telemetry.renderer.endpoint_id(),
            "deviceFormat": telemetry.renderer.device_format(),
            "sampleRate": telemetry.renderer.device_sample_rate,
            "channels": telemetry.renderer.device_channels,
            "bitsPerSample": telemetry.renderer.device_bits_per_sample,
            "validBitsPerSample": telemetry.renderer.device_valid_bits_per_sample,
            "channelMask": telemetry.renderer.device_channel_mask,
            "initializationPath": format!("{:?}", telemetry.renderer.initialization_path),
            "conversionActive": telemetry.renderer.conversion_active,
            "bufferFrames": telemetry.renderer.buffer_frames,
            "midstreamUnderruns": telemetry.audio_midstream_underruns,
            "terminalPlayoutEpisodes": telemetry.audio_terminal_playout_episodes,
        })),
        "playback": session.map(|telemetry| serde_json::json!({
            "decodedEligibleFrames": telemetry.metrics.decoded_eligible_frames,
            "presentedFrames": telemetry.metrics.presented_frames,
            "lateFrames": telemetry.metrics.late_frames,
            "schedulerDroppedFrames": telemetry.metrics.scheduler_dropped_frames,
            "lateOrDroppedFrames": telemetry.metrics.late_or_dropped_frames,
            "lateDropRatio": telemetry.metrics.late_drop_ratio(),
            "presentationBackpressuredFrames": telemetry.metrics.presentation_backpressured_frames,
            "presentationOccludedFrames": telemetry.metrics.presentation_occluded_frames,
            "unmeasuredPresentations": telemetry.metrics.unmeasured_presentations,
            "avErrorP95Ms": telemetry.metrics.av_error_histogram.percentile_millis(95),
            "avErrorHistogramOverflowed": telemetry.metrics.av_error_histogram.overflow() != 0,
            "avErrorHistogramSamples": telemetry.metrics.av_error_histogram.total(),
            "maxAvErrorTicks": telemetry.metrics.max_av_error_ticks,
            "seekSettleP95Ms": telemetry.metrics.seek_latency_histogram.percentile_millis(95),
            "seekLatencyHistogramOverflowed": telemetry.metrics.seek_latency_histogram.overflow() != 0,
            "seekLatencyHistogramSamples": telemetry.metrics.seek_latency_histogram.total(),
            "settledSeeks": telemetry.metrics.settled_seeks,
            "occludedSettledSeeks": telemetry.metrics.occluded_settled_seeks,
            "staleResults": telemetry.metrics.stale_results,
            "mftSamplesReceived": telemetry.decoder_ownership.mft_samples_received,
            "mftSamplesReleased": telemetry.decoder_ownership.mft_samples_released,
            "videoEncodedCapacity": telemetry.video_buffers.encoded_capacity,
            "videoConvertedCapacity": telemetry.video_buffers.converted_capacity,
            "audioPacketCapacity": telemetry.audio_packets.packet_capacity,
            "audioQueueHighWaterFrames": telemetry.audio_mix.queue_high_water_frames,
        })),
    });
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    serde_json::to_writer_pretty(&mut file, &value)?;
    use std::io::Write;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod benchmark_probe_tests {
    use super::benchmark_shell_is_safe;

    #[test]
    fn benchmark_probe_requires_optimization_and_debug_assertions() {
        assert!(benchmark_shell_is_safe(true, "3"));
        assert!(benchmark_shell_is_safe(true, "s"));
        assert!(!benchmark_shell_is_safe(true, "0"));
        assert!(!benchmark_shell_is_safe(false, "3"));
    }
}
