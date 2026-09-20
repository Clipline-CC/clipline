use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clipline_capture::diagnostics::{
    emit_diagnostic, install_diagnostic_handler, CaptureDiagnostic, DiagnosticRateLimiter,
};

#[test]
fn rate_limiter_emits_first_event_then_reports_suppressed_count() {
    let start = Instant::now();
    let mut limiter = DiagnosticRateLimiter::new(Duration::from_secs(30));

    assert_eq!(limiter.observe(start), Some(0));
    assert_eq!(limiter.observe(start + Duration::from_secs(1)), None);
    assert_eq!(limiter.observe(start + Duration::from_secs(29)), None);
    assert_eq!(limiter.observe(start + Duration::from_secs(30)), Some(2));
    assert_eq!(limiter.observe(start + Duration::from_secs(31)), None);
    assert_eq!(limiter.observe(start + Duration::from_secs(60)), Some(1));
}

#[test]
fn typed_capture_diagnostic_routes_through_installed_handler() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let received_by_handler = Arc::clone(&received);
    install_diagnostic_handler(move |event| {
        received_by_handler.lock().unwrap().push(event);
    })
    .expect("install one capture diagnostic handler");

    emit_diagnostic(CaptureDiagnostic::WasapiDataDiscontinuity {
        source: "output".into(),
        suppressed_since_last: 7,
    });

    let events = received.lock().unwrap();
    assert_eq!(
        events.as_slice(),
        [CaptureDiagnostic::WasapiDataDiscontinuity {
            source: "output".into(),
            suppressed_since_last: 7,
        }]
    );
    assert_eq!(
        events[0].to_string(),
        "capture event=wasapi_data_discontinuity source=output suppressed_since_last=7 action=audio_gap_fill_capped"
    );
}

#[test]
fn late_audio_reanchor_diagnostic_names_source_delay_and_suppression() {
    let event = CaptureDiagnostic::WasapiLateAudioReanchored {
        source: "microphone".into(),
        correction_ms: 37,
        total_correction_ms: 143,
        chunk_ms: 10,
        suppressed_since_last: 4,
    };

    assert_eq!(
        event.to_string(),
        "capture event=wasapi_late_audio_reanchored source=microphone correction_ms=37 total_correction_ms=143 chunk_ms=10 suppressed_since_last=4 action=preserve_live_audio"
    );
}
