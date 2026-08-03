//! Bounded JPEG-to-Slint handoff for native gallery posters.

use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::{
    mpsc::{self, SyncSender, TrySendError},
    Arc, Mutex, OnceLock,
};

use clipline_library::{
    PosterController, PosterControllerError, PosterControllerUpdate, PosterFailureKind,
    PosterWorkKind, PosterWorkRequest, MAX_DECODED_PAGE_IMAGES, MAX_POSTER_DECODED_PIXELS,
    MAX_POSTER_DECODER_ALLOC_BYTES, MAX_POSTER_DIMENSION, MAX_POSTER_ENCODED_BYTES,
    MAX_POSTER_RGB_BYTES,
};
use image::{ImageFormat, ImageReader, Limits};
use slint::{Rgb8Pixel, SharedPixelBuffer};

use crate::CliplineSpike;

thread_local! {
    static UI_POSTERS: std::cell::RefCell<PosterController<slint::Image>> =
        std::cell::RefCell::new(PosterController::new());
}

const POSTER_DECODE_WORKERS: usize = 2;
const POSTER_DECODE_QUEUE_CAPACITY: usize = MAX_DECODED_PAGE_IMAGES;
const _: () = assert!(POSTER_DECODE_WORKERS > 0);
const _: () = assert!(POSTER_DECODE_QUEUE_CAPACITY >= POSTER_DECODE_WORKERS);

struct PosterDecodeJob {
    window: slint::Weak<CliplineSpike>,
    request: PosterWorkRequest,
}

struct PosterDecodePool {
    sender: SyncSender<PosterDecodeJob>,
    _workers: Vec<std::thread::JoinHandle<()>>,
}

static POSTER_DECODE_POOL: OnceLock<Result<PosterDecodePool, String>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PosterAdapterError {
    NotDecodeWork,
    InvalidRequest,
    Open,
    Identity,
    EncodedTooLarge,
    Read,
    CorruptJpeg,
    InvalidDimensions,
    PixelLimit,
    RgbByteLimit,
    Decode,
}

impl std::fmt::Display for PosterAdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::NotDecodeWork => "poster request is not decode work",
            Self::InvalidRequest => "poster request does not own the encoded sidecar",
            Self::Open => "poster file could not be opened safely",
            Self::Identity => "poster file identity changed during decode",
            Self::EncodedTooLarge => "poster encoded bytes exceed their limit",
            Self::Read => "poster encoded bytes could not be read",
            Self::CorruptJpeg => "poster is not a decodable JPEG",
            Self::InvalidDimensions => "poster dimensions are invalid",
            Self::PixelLimit => "poster decoded pixels exceed their limit",
            Self::RgbByteLimit => "poster RGB bytes exceed their limit",
            Self::Decode => "poster JPEG decode failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PosterAdapterError {}

#[derive(Debug)]
pub struct DecodedPoster {
    request: PosterWorkRequest,
    pixels: SharedPixelBuffer<Rgb8Pixel>,
}

impl DecodedPoster {
    #[must_use]
    pub fn request(&self) -> &PosterWorkRequest {
        &self.request
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        self.pixels.width()
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        self.pixels.height()
    }
}

/// Open, bound, preflight, and decode one exact native poster file.
///
/// The returned value contains pixels only: every file and decoder handle has
/// been released before it can cross to the Slint event loop.
pub fn decode_poster_file(request: PosterWorkRequest) -> Result<DecodedPoster, PosterAdapterError> {
    let encoded_path = match &request.kind {
        PosterWorkKind::Decode { encoded_path } => encoded_path,
        PosterWorkKind::Extract => return Err(PosterAdapterError::NotDecodeWork),
    };
    if request.token.path != request.item.identity
        || clipline_library::ClipPathIdentity::from_path(&request.item.native_path).as_ref()
            != Some(&request.item.identity)
        || encoded_path != &clipline_library::poster_path(&request.item.native_path)
    {
        return Err(PosterAdapterError::InvalidRequest);
    }
    let (encoded, opened_identity) = read_exact_poster(encoded_path)?;
    let (width, height, expected_rgb_bytes) = match jpeg_dimensions(&encoded) {
        Ok(dimensions) => dimensions,
        Err(error) => {
            let _ = clipline_shell::remove_file_if_identity(encoded_path, opened_identity);
            return Err(error);
        }
    };

    let mut reader = ImageReader::with_format(Cursor::new(encoded), ImageFormat::Jpeg);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_POSTER_DIMENSION);
    limits.max_image_height = Some(MAX_POSTER_DIMENSION);
    limits.max_alloc = Some(MAX_POSTER_DECODER_ALLOC_BYTES);
    reader.limits(limits);
    let rgb = match reader.decode() {
        Ok(image) => image.into_rgb8(),
        Err(_) => {
            let _ = clipline_shell::remove_file_if_identity(encoded_path, opened_identity);
            return Err(PosterAdapterError::Decode);
        }
    };
    if rgb.width() != width || rgb.height() != height || rgb.as_raw().len() != expected_rgb_bytes {
        let _ = clipline_shell::remove_file_if_identity(encoded_path, opened_identity);
        return Err(PosterAdapterError::Decode);
    }

    let current = clipline_shell::open_regular_file_nofollow(encoded_path)
        .map_err(|_| PosterAdapterError::Identity)?;
    if clipline_shell::opened_file_identity(&current).map_err(|_| PosterAdapterError::Identity)?
        != opened_identity
    {
        return Err(PosterAdapterError::Identity);
    }
    drop(current);

    let pixels = SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(rgb.as_raw(), width, height);
    drop(rgb);
    Ok(DecodedPoster { request, pixels })
}

/// Perform the final exact-token check on the UI thread before constructing
/// the non-Send `slint::Image` handle.
pub fn publish_decoded_poster(
    controller: &mut PosterController<slint::Image>,
    decoded: DecodedPoster,
) -> Result<PosterControllerUpdate<slint::Image>, PosterControllerError> {
    let DecodedPoster { request, pixels } = decoded;
    controller.accept_decoded_with(&request, || slint::Image::from_rgb8(pixels))
}

/// Access the one UI-thread poster owner used by the single native main
/// window. Shell detach/hide code uses this seam to synchronously release
/// retained image handles before destroying the component.
pub fn with_ui_poster_controller<R>(
    operation: impl FnOnce(&mut PosterController<slint::Image>) -> R,
) -> R {
    UI_POSTERS.with(|controller| operation(&mut controller.borrow_mut()))
}

/// Clone one exact decoded image retained by the bounded UI-thread owner.
///
/// The controller retains at most [`MAX_DECODED_PAGE_IMAGES`] handles, and a
/// pending or stale identity never produces a clone.
#[must_use]
pub fn clone_ui_poster(identity: &clipline_library::ClipPathIdentity) -> Option<slint::Image> {
    with_ui_poster_controller(|controller| controller.retained_image(identity).cloned())
}

/// Return the fixed worker and queue bounds used by the poster decoder.
#[must_use]
pub const fn poster_decode_pool_shape() -> (usize, usize) {
    (POSTER_DECODE_WORKERS, POSTER_DECODE_QUEUE_CAPACITY)
}

/// Enqueue decoding on the fixed bounded worker pool and post only owned
/// pixels plus an exact request token to the Slint event loop. A missing weak
/// component still completes the controller lease so window destruction
/// cannot strand the 32-slot budget.
pub fn spawn_poster_decode(
    window: slint::Weak<CliplineSpike>,
    request: PosterWorkRequest,
) -> std::io::Result<()> {
    let pool = match poster_decode_pool() {
        Ok(pool) => pool,
        Err(error) => {
            let _ = dispatch_decode_failure(window, request, PosterFailureKind::Unavailable);
            return Err(error);
        }
    };
    match pool.sender.try_send(PosterDecodeJob { window, request }) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(job)) => {
            let _ =
                dispatch_decode_failure(job.window, job.request, PosterFailureKind::Unavailable);
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "poster decode queue is full",
            ))
        }
        Err(TrySendError::Disconnected(job)) => {
            let _ =
                dispatch_decode_failure(job.window, job.request, PosterFailureKind::Unavailable);
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "poster decode workers stopped",
            ))
        }
    }
}

fn poster_decode_pool() -> std::io::Result<&'static PosterDecodePool> {
    match POSTER_DECODE_POOL.get_or_init(PosterDecodePool::start) {
        Ok(pool) => Ok(pool),
        Err(message) => Err(std::io::Error::other(message.clone())),
    }
}

impl PosterDecodePool {
    fn start() -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(POSTER_DECODE_QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::new();
        workers
            .try_reserve_exact(POSTER_DECODE_WORKERS)
            .map_err(|error| format!("poster decode worker allocation failed: {error}"))?;

        for index in 0..POSTER_DECODE_WORKERS {
            let worker_receiver = Arc::clone(&receiver);
            match std::thread::Builder::new()
                .name(format!("clipline-poster-decode-{index}"))
                .spawn(move || poster_decode_worker(&worker_receiver))
            {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    drop(sender);
                    drop(receiver);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(format!("poster decode worker could not start: {error}"));
                }
            }
        }
        Ok(Self {
            sender,
            _workers: workers,
        })
    }
}

fn poster_decode_worker(receiver: &Mutex<mpsc::Receiver<PosterDecodeJob>>) {
    loop {
        let job = {
            let receiver = receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            receiver.recv()
        };
        let Ok(PosterDecodeJob { window, request }) = job else {
            return;
        };
        match decode_poster_file(request.clone()) {
            Ok(decoded) => {
                let _ = dispatch_decoded_poster(window, decoded);
            }
            Err(error) => {
                let _ = dispatch_decode_failure(window, request, failure_kind(error));
            }
        }
    }
}

fn dispatch_decoded_poster(
    window: slint::Weak<CliplineSpike>,
    decoded: DecodedPoster,
) -> Result<(), slint::EventLoopError> {
    slint::invoke_from_event_loop(move || {
        with_ui_poster_controller(|controller| {
            if window.upgrade().is_some() {
                let _ = publish_decoded_poster(controller, decoded);
            } else {
                let _ = controller.detach_window_if_matches(decoded.request().token.window);
                let _ = controller.acknowledge_canceled(decoded.request());
            }
        });
    })
}

fn dispatch_decode_failure(
    window: slint::Weak<CliplineSpike>,
    request: PosterWorkRequest,
    failure: PosterFailureKind,
) -> Result<(), slint::EventLoopError> {
    slint::invoke_from_event_loop(move || {
        with_ui_poster_controller(|controller| {
            if window.upgrade().is_some() {
                let _ = controller.accept_decode_failed(&request, failure);
            } else {
                let _ = controller.detach_window_if_matches(request.token.window);
                let _ = controller.acknowledge_canceled(&request);
            }
        });
    })
}

fn failure_kind(error: PosterAdapterError) -> PosterFailureKind {
    match error {
        PosterAdapterError::EncodedTooLarge
        | PosterAdapterError::CorruptJpeg
        | PosterAdapterError::InvalidDimensions
        | PosterAdapterError::PixelLimit
        | PosterAdapterError::RgbByteLimit
        | PosterAdapterError::Decode
        | PosterAdapterError::NotDecodeWork
        | PosterAdapterError::InvalidRequest => PosterFailureKind::Corrupt,
        PosterAdapterError::Open => PosterFailureKind::Unavailable,
        PosterAdapterError::Identity | PosterAdapterError::Read => PosterFailureKind::Io,
    }
}

fn read_exact_poster(
    path: &Path,
) -> Result<(Vec<u8>, clipline_shell::FileIdentity), PosterAdapterError> {
    let file =
        clipline_shell::open_regular_file_nofollow(path).map_err(|_| PosterAdapterError::Open)?;
    let identity =
        clipline_shell::opened_file_identity(&file).map_err(|_| PosterAdapterError::Identity)?;
    let encoded_len = file.metadata().map_err(|_| PosterAdapterError::Read)?.len();
    if encoded_len > MAX_POSTER_ENCODED_BYTES as u64 {
        drop(file);
        let _ = clipline_shell::remove_file_if_identity(path, identity);
        return Err(PosterAdapterError::EncodedTooLarge);
    }
    let capacity = usize::try_from(encoded_len).map_err(|_| PosterAdapterError::EncodedTooLarge)?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(capacity.min(MAX_POSTER_ENCODED_BYTES))
        .map_err(|_| PosterAdapterError::EncodedTooLarge)?;
    file.take(MAX_POSTER_ENCODED_BYTES as u64 + 1)
        .read_to_end(&mut encoded)
        .map_err(|_| PosterAdapterError::Read)?;
    if encoded.len() > MAX_POSTER_ENCODED_BYTES {
        let _ = clipline_shell::remove_file_if_identity(path, identity);
        return Err(PosterAdapterError::EncodedTooLarge);
    }
    Ok((encoded, identity))
}

fn jpeg_dimensions(encoded: &[u8]) -> Result<(u32, u32, usize), PosterAdapterError> {
    let reader = ImageReader::with_format(Cursor::new(encoded), ImageFormat::Jpeg);
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| PosterAdapterError::CorruptJpeg)?;
    validate_poster_dimensions(width, height)
}

pub fn validate_poster_dimensions(
    width: u32,
    height: u32,
) -> Result<(u32, u32, usize), PosterAdapterError> {
    if width == 0 || height == 0 || width > MAX_POSTER_DIMENSION || height > MAX_POSTER_DIMENSION {
        return Err(PosterAdapterError::InvalidDimensions);
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(PosterAdapterError::PixelLimit)?;
    if pixels > MAX_POSTER_DECODED_PIXELS {
        return Err(PosterAdapterError::PixelLimit);
    }
    let rgb_bytes = pixels
        .checked_mul(3)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(PosterAdapterError::RgbByteLimit)?;
    if rgb_bytes > MAX_POSTER_RGB_BYTES {
        return Err(PosterAdapterError::RgbByteLimit);
    }
    Ok((width, height, rgb_bytes))
}
