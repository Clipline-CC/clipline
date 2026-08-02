use clipline_slint_spike::options::{OptionsError, SpikeOptions};
#[cfg(not(windows))]
use slint::ComponentHandle;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if print_package_probe_if_requested()? {
        return Ok(());
    }
    if print_benchmark_probe_if_requested() {
        return Ok(());
    }
    let raw_arguments = std::env::args_os().collect::<Vec<_>>();
    let shell_arguments = raw_arguments
        .iter()
        .map(|argument| {
            argument
                .to_str()
                .map(str::to_owned)
                .ok_or("Clipline shell arguments must be valid Unicode")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let launch = clipline_shell::ShellLaunch::parse(&shell_arguments)?;
    let mut application_arguments = vec![raw_arguments
        .first()
        .cloned()
        .ok_or("Clipline shell arguments must include the executable")?];
    application_arguments.extend(
        launch
            .application_arguments()
            .iter()
            .map(std::ffi::OsString::from),
    );
    let mut options = match SpikeOptions::parse(application_arguments) {
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
    options.autostart = launch.mode() == clipline_shell::LaunchMode::Autostart;
    std::env::set_var("SLINT_BACKEND", &options.renderer);
    run(options)
}

const BENCHMARK_PROBE_ARGUMENT: &str = "--clipline-benchmark-probe";
const PACKAGE_PROBE_ARGUMENT: &str = "--clipline-package-probe";

fn print_package_probe_if_requested() -> Result<bool, Box<dyn std::error::Error>> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let requested = arguments
        .iter()
        .skip(1)
        .any(|argument| argument == PACKAGE_PROBE_ARGUMENT);
    if !requested {
        return Ok(false);
    }
    if arguments.len() != 2 || arguments[1] != PACKAGE_PROBE_ARGUMENT {
        return Err("--clipline-package-probe must be the only application argument".into());
    }
    println!(
        "{}",
        serde_json::json!({
            "schemaVersion": 1,
            "kind": "clipline-slint-internal-candidate",
            "productName": "Clipline",
            "publisher": "Clipline",
            "identifier": "io.clipline.app",
            "version": env!("CLIPLINE_PACKAGE_VERSION"),
            "variant": env!("CLIPLINE_PACKAGE_VARIANT"),
            "applicationStateStarted": false,
            "autostartRegistryMutation": false,
        })
    );
    Ok(true)
}

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
    let desktop_adapter = clipline_slint_spike::desktop::SlintDesktopAdapter::start_detached()
        .map_err(std::io::Error::other)?;
    let _desktop_attachment = desktop_adapter
        .attach(window.as_weak())
        .map_err(std::io::Error::other)?;
    window.run()?;
    drop(desktop_adapter);
    Ok(())
}

#[cfg(windows)]
fn run(options: SpikeOptions) -> Result<(), Box<dyn std::error::Error>> {
    let telemetry_path = options.telemetry_path.clone();
    let report =
        clipline_slint_spike::shell::run_windows_shell(options).map_err(std::io::Error::other)?;
    if let Some(path) = telemetry_path.as_ref() {
        write_run_telemetry(
            path,
            report.latest_session.as_ref(),
            report.lifecycle,
            report.resources,
        )?;
    }
    Ok(())
}

#[cfg(windows)]
fn write_run_telemetry(
    path: &std::path::Path,
    report: Option<&clipline_slint_spike::live::LiveSessionReport>,
    lifecycle: clipline_slint_spike::shell::LifecycleSnapshot,
    resources: clipline_slint_spike::shell::ShellResourceSnapshot,
) -> Result<(), Box<dyn std::error::Error>> {
    use clipline_slint_spike::live::PresentationTelemetry;

    let presentation = match report.map(|report| report.presentation) {
        Some(PresentationTelemetry::D3d(telemetry)) => serde_json::json!({
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
        Some(PresentationTelemetry::Cpu {
            frames,
            readback_frames,
            max_copy_time_100ns,
        }) => serde_json::json!({
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
        None => serde_json::json!({ "path": "none" }),
    };
    let session = report.and_then(|report| report.telemetry.as_ref());
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
        "sessionExit": report.map_or_else(|| "tray-only".to_owned(), |report| format!("{:?}", report.exit).to_lowercase()),
        "lifecycle": {
            "trayReady": resources.tray_ready,
            "desktopConsumerAlive": resources.desktop_consumer_alive,
            "hotkeyServiceAlive": resources.hotkey_service_alive,
            "activationServiceAlive": resources.activation_service_alive,
            "mode": format!("{:?}", lifecycle.mode).to_lowercase(),
            "windowActive": lifecycle.window_active,
            "quitting": lifecycle.quitting,
            "attachmentGeneration": lifecycle.attachment_generation,
            "openAccepted": lifecycle.counters.open_requests,
            "closeAccepted": lifecycle.counters.close_requests,
            "windowCreated": lifecycle.counters.windows_created,
            "windowDropped": lifecycle.counters.windows_dropped,
            "maxLiveWindows": resources.max_live_windows,
            "desktopAttached": resources.desktop_attached,
            "desktopDetached": resources.desktop_detached,
            "playbackStarted": resources.playback_started,
            "playbackStopped": resources.playback_stopped,
            "videoHostCreated": resources.video_hosts_created,
            "videoHostDropped": resources.video_hosts_dropped,
            "liveDesktopAttachments": resources.live_desktop_attachments,
            "livePlaybackSessions": resources.live_playback_sessions,
            "liveVideoHosts": resources.live_video_hosts,
            "modelSetsCreated": resources.model_sets_created,
            "modelSetsDropped": resources.model_sets_dropped,
            "liveModelSets": resources.live_model_sets,
            "presentationResourcesLive": resources.live_playback_sessions + resources.live_video_hosts + resources.live_model_sets,
            "staleClosuresRejected": lifecycle.counters.stale_callbacks,
            "quitAccepted": lifecycle.counters.quit_effects,
        },
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
