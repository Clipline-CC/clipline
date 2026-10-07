//! Previews for the custom-game window picker. One worker thread owns the
//! picker's D3D device and serves requests in order, so at most one native
//! capture runs at a time.

use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use clipline_capture::windows::d3d11;
use clipline_capture::windows::window_preview::capture_window_preview;

const PREVIEW_WIDTH: u32 = 320;
const PREVIEW_HEIGHT: u32 = 180;
/// Frame wait per window; readback and encoding come on top (~50 ms).
const PREVIEW_TIMEOUT: Duration = Duration::from_millis(750);

struct Request {
    handle: isize,
    process_id: u32,
    reply: Sender<Option<String>>,
}

/// A PNG data URL previewing the window, or `None` when the picker should
/// show the app icon instead. Validate the handle/process pair first.
pub(crate) fn window_preview_data_url(handle: isize, process_id: u32) -> Option<String> {
    let (reply, response) = mpsc::channel();
    worker()
        .lock()
        .ok()?
        .send(Request {
            handle,
            process_id,
            reply,
        })
        .ok()?;
    response.recv().ok().flatten()
}

fn worker() -> &'static Mutex<Sender<Request>> {
    static WORKER: OnceLock<Mutex<Sender<Request>>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (requests, inbox) = mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name("clipline-window-previews".into())
            .spawn(move || {
                let device = match d3d11::create_device() {
                    Ok((device, _)) => Some(device),
                    Err(error) => {
                        tracing::warn!(event = "window_preview_device_failed", error = %error);
                        None
                    }
                };
                for request in inbox {
                    let preview = device.as_ref().and_then(|device| {
                        capture_window_preview(
                            device,
                            request.handle,
                            request.process_id,
                            PREVIEW_WIDTH,
                            PREVIEW_HEIGHT,
                            PREVIEW_TIMEOUT,
                        )
                        .unwrap_or_else(|error| {
                            tracing::debug!(event = "window_preview_failed", error = %error);
                            None
                        })
                    });
                    let url = preview
                        .and_then(|thumbnail| {
                            crate::game_icon::encode_rgba_png(
                                thumbnail.width,
                                thumbnail.height,
                                &thumbnail.rgba,
                            )
                        })
                        .map(|png| crate::game_icon::png_data_url(&png));
                    let _ = request.reply.send(url);
                }
            })
            .expect("spawn window preview worker");
        Mutex::new(requests)
    })
}
