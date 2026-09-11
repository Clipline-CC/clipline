use std::fmt;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureDiagnostic {
    HybridCanvasSelected {
        client_size: Option<(u32, u32)>,
        display_size: (u32, u32),
        canvas_size: (u32, u32),
        reason: &'static str,
    },
    HybridSourceChanged {
        source: &'static str,
    },
    DxgiReopenFailed {
        hresult: i32,
        suppressed_since_last: u64,
    },
    WasapiDataDiscontinuity {
        suppressed_since_last: u64,
    },
    WasapiLateAudioReanchored {
        source: &'static str,
        correction_ms: u64,
        total_correction_ms: u64,
        chunk_ms: u64,
        suppressed_since_last: u64,
    },
    WasapiDeviceLost {
        source: &'static str,
        hresult: i32,
        suppressed_since_last: u64,
    },
    WasapiDeviceRecovered {
        source: &'static str,
        outage_ms: u64,
    },
}

impl fmt::Display for CaptureDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HybridCanvasSelected {
                client_size,
                display_size,
                canvas_size,
                reason,
            } => write!(
                formatter,
                "capture event=experimental_hybrid_canvas_selected client_size={client_size:?} display_size={display_size:?} canvas_size={canvas_size:?} reason={reason}"
            ),
            Self::HybridSourceChanged { source } => write!(
                formatter,
                "capture event=experimental_hybrid_source_changed source={source}"
            ),
            Self::DxgiReopenFailed {
                hresult,
                suppressed_since_last,
            } => write!(
                formatter,
                "capture event=dxgi_reopen_failed hresult=0x{hresult:08x} suppressed_since_last={suppressed_since_last} action=retry_same_output"
            ),
            Self::WasapiDataDiscontinuity {
                suppressed_since_last,
            } => write!(
                formatter,
                "capture event=wasapi_data_discontinuity suppressed_since_last={suppressed_since_last} action=audio_gap_fill_capped"
            ),
            Self::WasapiLateAudioReanchored {
                source,
                correction_ms,
                total_correction_ms,
                chunk_ms,
                suppressed_since_last,
            } => write!(
                formatter,
                "capture event=wasapi_late_audio_reanchored source={source} correction_ms={correction_ms} total_correction_ms={total_correction_ms} chunk_ms={chunk_ms} suppressed_since_last={suppressed_since_last} action=preserve_live_audio"
            ),
            Self::WasapiDeviceLost {
                source,
                hresult,
                suppressed_since_last,
            } => write!(
                formatter,
                "capture event=wasapi_device_lost source={source} hresult=0x{hresult:08x} suppressed_since_last={suppressed_since_last} action=silence_until_device_returns"
            ),
            Self::WasapiDeviceRecovered { source, outage_ms } => write!(
                formatter,
                "capture event=wasapi_device_recovered source={source} outage_ms={outage_ms} action=resume_live_capture"
            ),
        }
    }
}

type DiagnosticHandler = dyn Fn(CaptureDiagnostic) + Send + Sync + 'static;
static DIAGNOSTIC_HANDLER: OnceLock<Box<DiagnosticHandler>> = OnceLock::new();

pub fn install_diagnostic_handler(
    handler: impl Fn(CaptureDiagnostic) + Send + Sync + 'static,
) -> Result<(), &'static str> {
    DIAGNOSTIC_HANDLER
        .set(Box::new(handler))
        .map_err(|_| "capture diagnostic handler is already installed")
}

pub fn emit_diagnostic(event: CaptureDiagnostic) {
    if let Some(handler) = DIAGNOSTIC_HANDLER.get() {
        handler(event);
    }
}

#[derive(Debug)]
pub struct DiagnosticRateLimiter {
    interval: Duration,
    last_emitted: Option<Instant>,
    suppressed: u64,
}

impl DiagnosticRateLimiter {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_emitted: None,
            suppressed: 0,
        }
    }

    pub fn observe(&mut self, now: Instant) -> Option<u64> {
        let should_emit = self
            .last_emitted
            .is_none_or(|last| now.saturating_duration_since(last) >= self.interval);
        if should_emit {
            self.last_emitted = Some(now);
            return Some(std::mem::take(&mut self.suppressed));
        }
        self.suppressed = self.suppressed.saturating_add(1);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_diagnostic_distinguishes_missing_geometry_from_real_ultrawide() {
        let render = |client_size, reason| {
            CaptureDiagnostic::HybridCanvasSelected {
                client_size,
                display_size: (5120, 1440),
                canvas_size: (5120, 1440),
                reason,
            }
            .to_string()
        };
        let missing = render(None, "client_unavailable");
        let full = render(Some((5120, 1440)), "client_aspect_monitor_budget");
        assert!(missing.contains("client_size=None"));
        assert!(missing.contains("reason=client_unavailable"));
        assert!(full.contains("client_size=Some((5120, 1440))"));
        assert!(full.contains("reason=client_aspect_monitor_budget"));
        for text in [missing, full] {
            assert!(text.contains("display_size=(5120, 1440)"));
            assert!(text.contains("canvas_size=(5120, 1440)"));
        }
    }

    #[test]
    fn device_loss_display_reports_source_hresult_and_action() {
        let event = CaptureDiagnostic::WasapiDeviceLost {
            source: "output",
            hresult: 0x88890004u32 as i32,
            suppressed_since_last: 2,
        };
        let text = event.to_string();
        assert!(text.contains("event=wasapi_device_lost"));
        assert!(text.contains("source=output"));
        assert!(text.contains("hresult=0x88890004"));
        assert!(text.contains("suppressed_since_last=2"));
        assert!(text.contains("action="));
    }

    #[test]
    fn device_recovery_display_reports_outage_and_action() {
        let event = CaptureDiagnostic::WasapiDeviceRecovered {
            source: "microphone",
            outage_ms: 12_500,
        };
        let text = event.to_string();
        assert!(text.contains("event=wasapi_device_recovered"));
        assert!(text.contains("source=microphone"));
        assert!(text.contains("outage_ms=12500"));
        assert!(text.contains("action="));
    }
}
