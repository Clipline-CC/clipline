//! Bounded JPEG-to-Slint handoff for native gallery posters.

use std::collections::VecDeque;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::{
    mpsc::{self, SyncSender, TrySendError},
    Arc, Mutex, OnceLock,
};

use clipline_library::{
    CatalogResult, CatalogResultSender, ExpectedResultOwner, PosterCompletion, PosterController,
    PosterControllerError, PosterControllerUpdate, PosterFailureKind, PosterPageItem, PosterResult,
    PosterService, PosterStatus, PosterWorkKind, PosterWorkRequest, WindowWorkToken,
    MAX_DECODED_PAGE_IMAGES, MAX_POSTER_DECODED_PIXELS, MAX_POSTER_DECODER_ALLOC_BYTES,
    MAX_POSTER_DIMENSION, MAX_POSTER_ENCODED_BYTES, MAX_POSTER_RGB_BYTES,
};
use image::{ImageFormat, ImageReader, Limits};
use slint::{Rgb8Pixel, SharedPixelBuffer};

use crate::CliplineSpike;

thread_local! {
    static UI_POSTERS: std::cell::RefCell<PosterController<slint::Image>> =
        std::cell::RefCell::new(PosterController::new());
    static UI_POSTER_BINDING: std::cell::RefCell<Option<UiPosterBinding>> =
        const { std::cell::RefCell::new(None) };
}

const POSTER_DECODE_WORKERS: usize = 2;
const POSTER_DECODE_QUEUE_CAPACITY: usize = MAX_DECODED_PAGE_IMAGES;
const POSTER_EXTRACTION_WORKERS: usize = 2;
const POSTER_EXTRACTION_QUEUE_CAPACITY: usize = MAX_DECODED_PAGE_IMAGES;
const _: () = assert!(POSTER_DECODE_WORKERS > 0);
const _: () = assert!(POSTER_DECODE_QUEUE_CAPACITY >= POSTER_DECODE_WORKERS);

struct PosterDecodeJob {
    sink: PosterDecodeSink,
    request: PosterWorkRequest,
}

enum PosterDecodeSink {
    Direct(slint::Weak<CliplineSpike>),
    Catalog(CatalogResultSender),
}

#[derive(Clone)]
struct UiPosterBinding {
    token: WindowWorkToken,
    window: slint::Weak<CliplineSpike>,
    results: CatalogResultSender,
}

struct PosterDecodePool {
    sender: SyncSender<PosterDecodeJob>,
    _workers: Vec<std::thread::JoinHandle<()>>,
}

static POSTER_DECODE_POOL: OnceLock<Result<PosterDecodePool, String>> = OnceLock::new();

struct PosterExtractionJob {
    results: CatalogResultSender,
    request: PosterWorkRequest,
}

struct PosterExtractionPool {
    sender: SyncSender<PosterExtractionJob>,
    _workers: Vec<std::thread::JoinHandle<()>>,
}

static POSTER_EXTRACTION_POOL: OnceLock<Result<PosterExtractionPool, String>> = OnceLock::new();

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

/// Return the fixed process pool used for FFmpeg poster extraction.
#[must_use]
pub const fn poster_extraction_pool_shape() -> (usize, usize) {
    (POSTER_EXTRACTION_WORKERS, POSTER_EXTRACTION_QUEUE_CAPACITY)
}

/// Install one exact local page into the UI-thread poster owner and queue only
/// the bounded decoded-image viewport. All filesystem/process work remains on
/// fixed worker pools.
pub fn replace_ui_poster_page(
    window: slint::Weak<CliplineSpike>,
    results: CatalogResultSender,
    token: WindowWorkToken,
    page: Vec<PosterPageItem>,
    viewport_start: usize,
) -> Result<(), String> {
    let page_len = page.len();
    let mut update = with_ui_poster_controller(|controller| {
        controller
            .replace_page(token, page)
            .map_err(|error| error.to_string())
    })?;
    UI_POSTER_BINDING.with(|binding| {
        *binding.borrow_mut() = Some(UiPosterBinding {
            token,
            window,
            results,
        });
    });
    let viewport = with_ui_poster_controller(|controller| {
        controller
            .set_viewport(viewport_start.min(page_len), MAX_DECODED_PAGE_IMAGES, 0)
            .map_err(|error| error.to_string())
    })?;
    append_update(&mut update, viewport);
    route_catalog_poster_update(update)
}

/// Move the bounded decoded-image window without retaining off-viewport Slint
/// handles. The exact window token prevents an old scroll callback from
/// changing a recreated component.
pub fn update_ui_poster_viewport(
    token: WindowWorkToken,
    viewport_start: usize,
) -> Result<(), String> {
    if catalog_binding(token).is_none() {
        return Ok(());
    }
    let update = with_ui_poster_controller(|controller| {
        controller
            .set_viewport(viewport_start, MAX_DECODED_PAGE_IMAGES, 0)
            .map_err(|error| error.to_string())
    })?;
    route_catalog_poster_update(update)
}

/// Synchronously fence a closing Slint component and release every retained
/// image handle before its row model is cleared.
pub fn detach_ui_poster_window(token: WindowWorkToken) -> Result<(), String> {
    UI_POSTER_BINDING.with(|binding| {
        let mut binding = binding.borrow_mut();
        if binding
            .as_ref()
            .is_some_and(|binding| binding.token == token)
        {
            binding.take();
        }
    });
    let update = with_ui_poster_controller(|controller| {
        controller
            .detach_window_if_matches(token)
            .map_err(|error| error.to_string())
    })?;
    route_catalog_poster_update(update)
}

fn append_update(
    target: &mut PosterControllerUpdate<slint::Image>,
    mut update: PosterControllerUpdate<slint::Image>,
) {
    target.queued.append(&mut update.queued);
    target.canceled.append(&mut update.canceled);
    target.released.append(&mut update.released);
}

fn catalog_binding(token: WindowWorkToken) -> Option<UiPosterBinding> {
    UI_POSTER_BINDING.with(|binding| {
        binding
            .borrow()
            .as_ref()
            .filter(|binding| binding.token == token && binding.window.upgrade().is_some())
            .cloned()
    })
}

fn route_catalog_poster_update(update: PosterControllerUpdate<slint::Image>) -> Result<(), String> {
    let mut updates = VecDeque::from([update]);
    while let Some(update) = updates.pop_front() {
        drop(update.released);
        for request in update.canceled {
            let followup = with_ui_poster_controller(|controller| {
                controller
                    .acknowledge_canceled(&request)
                    .map_err(|error| error.to_string())
            })?;
            updates.push_back(followup);
        }
        for request in update.queued {
            let Some(binding) = catalog_binding(request.token.window) else {
                let followup = with_ui_poster_controller(|controller| {
                    controller
                        .acknowledge_canceled(&request)
                        .map_err(|error| error.to_string())
                })?;
                updates.push_back(followup);
                continue;
            };
            match request.kind {
                PosterWorkKind::Extract => {
                    spawn_catalog_poster_extract(binding.results, request)?;
                }
                PosterWorkKind::Decode { .. } => {
                    spawn_catalog_poster_decode(binding.results, request)?;
                }
            }
        }
    }
    Ok(())
}

fn spawn_catalog_poster_decode(
    results: CatalogResultSender,
    request: PosterWorkRequest,
) -> Result<(), String> {
    let pool = match poster_decode_pool() {
        Ok(pool) => pool,
        Err(_) => {
            return dispatch_catalog_decode_failure(
                results,
                request,
                PosterFailureKind::Unavailable,
            )
            .map_err(|error| error.to_string());
        }
    };
    match pool.sender.try_send(PosterDecodeJob {
        sink: PosterDecodeSink::Catalog(results.clone()),
        request,
    }) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) => {
            dispatch_catalog_decode_failure(results, job.request, PosterFailureKind::Unavailable)
                .map_err(|error| error.to_string())
        }
    }
}

fn spawn_catalog_poster_extract(
    results: CatalogResultSender,
    request: PosterWorkRequest,
) -> Result<(), String> {
    let pool = match poster_extraction_pool() {
        Ok(pool) => pool,
        Err(_) => {
            return dispatch_catalog_extracted(
                results,
                request,
                PosterCompletion::Failed(PosterFailureKind::Unavailable),
            )
            .map_err(|error| error.to_string());
        }
    };
    match pool.sender.try_send(PosterExtractionJob {
        results: results.clone(),
        request,
    }) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) => {
            dispatch_catalog_extracted(
                results,
                job.request,
                PosterCompletion::Failed(PosterFailureKind::Unavailable),
            )
            .map_err(|error| error.to_string())
        }
    }
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
    match pool.sender.try_send(PosterDecodeJob {
        sink: PosterDecodeSink::Direct(window),
        request,
    }) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(job)) => {
            if let PosterDecodeSink::Direct(window) = job.sink {
                let _ =
                    dispatch_decode_failure(window, job.request, PosterFailureKind::Unavailable);
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "poster decode queue is full",
            ))
        }
        Err(TrySendError::Disconnected(job)) => {
            if let PosterDecodeSink::Direct(window) = job.sink {
                let _ =
                    dispatch_decode_failure(window, job.request, PosterFailureKind::Unavailable);
            }
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

fn poster_extraction_pool() -> std::io::Result<&'static PosterExtractionPool> {
    match POSTER_EXTRACTION_POOL.get_or_init(PosterExtractionPool::start) {
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

impl PosterExtractionPool {
    fn start() -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(POSTER_EXTRACTION_QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let service = Arc::new(PosterService::standard());
        let mut workers = Vec::new();
        workers
            .try_reserve_exact(POSTER_EXTRACTION_WORKERS)
            .map_err(|error| format!("poster extraction worker allocation failed: {error}"))?;
        for index in 0..POSTER_EXTRACTION_WORKERS {
            let worker_receiver = Arc::clone(&receiver);
            let service = Arc::clone(&service);
            match std::thread::Builder::new()
                .name(format!("clipline-poster-extract-{index}"))
                .spawn(move || poster_extraction_worker(&worker_receiver, &service))
            {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    drop(sender);
                    drop(receiver);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(format!("poster extraction worker could not start: {error}"));
                }
            }
        }
        Ok(Self {
            sender,
            _workers: workers,
        })
    }
}

fn poster_extraction_worker(
    receiver: &Mutex<mpsc::Receiver<PosterExtractionJob>>,
    service: &PosterService,
) {
    loop {
        let job = {
            let receiver = receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            receiver.recv()
        };
        let Ok(PosterExtractionJob { results, request }) = job else {
            return;
        };
        let completion =
            match service.ensure_poster(&request.item.native_path, request.item.seek_seconds) {
                Ok(path) => PosterCompletion::Ready(path),
                Err(message) if message.to_ascii_lowercase().contains("timed out") => {
                    PosterCompletion::Failed(PosterFailureKind::TimedOut)
                }
                Err(_) => PosterCompletion::Failed(PosterFailureKind::Unavailable),
            };
        let _ = dispatch_catalog_extracted(results, request, completion);
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
        let Ok(PosterDecodeJob { sink, request }) = job else {
            return;
        };
        match decode_poster_file(request.clone()) {
            Ok(decoded) => match sink {
                PosterDecodeSink::Direct(window) => {
                    let _ = dispatch_decoded_poster(window, decoded);
                }
                PosterDecodeSink::Catalog(results) => {
                    let _ = dispatch_catalog_decoded_poster(results, decoded);
                }
            },
            Err(error) => match sink {
                PosterDecodeSink::Direct(window) => {
                    let _ = dispatch_decode_failure(window, request, failure_kind(error));
                }
                PosterDecodeSink::Catalog(results) => {
                    let _ = dispatch_catalog_decode_failure(results, request, failure_kind(error));
                }
            },
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

fn dispatch_catalog_extracted(
    results: CatalogResultSender,
    request: PosterWorkRequest,
    completion: PosterCompletion,
) -> Result<(), slint::EventLoopError> {
    slint::invoke_from_event_loop(move || {
        let accepted = with_ui_poster_controller(|controller| controller.accepts_request(&request));
        let status = completion_status(&completion);
        let update = with_ui_poster_controller(|controller| {
            controller.accept_extracted(&request, completion)
        });
        if accepted {
            publish_catalog_poster_status(results, &request, status);
        }
        let _ = route_catalog_poster_update(update);
    })
}

fn dispatch_catalog_decoded_poster(
    results: CatalogResultSender,
    decoded: DecodedPoster,
) -> Result<(), slint::EventLoopError> {
    slint::invoke_from_event_loop(move || {
        let request = decoded.request().clone();
        let accepted = with_ui_poster_controller(|controller| controller.accepts_request(&request));
        let update = with_ui_poster_controller(|controller| {
            publish_decoded_poster(controller, decoded).unwrap_or_default()
        });
        if accepted {
            let path = match &request.kind {
                PosterWorkKind::Decode { encoded_path } => encoded_path.display().to_string(),
                PosterWorkKind::Extract => String::new(),
            };
            publish_catalog_poster_status(results, &request, PosterStatus::Ready { path });
        }
        let _ = route_catalog_poster_update(update);
    })
}

fn dispatch_catalog_decode_failure(
    results: CatalogResultSender,
    request: PosterWorkRequest,
    failure: PosterFailureKind,
) -> Result<(), slint::EventLoopError> {
    slint::invoke_from_event_loop(move || {
        let accepted = with_ui_poster_controller(|controller| controller.accepts_request(&request));
        let update = with_ui_poster_controller(|controller| {
            controller
                .accept_decode_failed(&request, failure)
                .unwrap_or_default()
        });
        if accepted {
            publish_catalog_poster_status(results, &request, failed_status(failure));
        }
        let _ = route_catalog_poster_update(update);
    })
}

fn completion_status(completion: &PosterCompletion) -> PosterStatus {
    match completion {
        PosterCompletion::Ready(path) => PosterStatus::Ready {
            path: path.display().to_string(),
        },
        PosterCompletion::Missing => PosterStatus::Missing,
        PosterCompletion::Failed(failure) => failed_status(*failure),
    }
}

fn failed_status(failure: PosterFailureKind) -> PosterStatus {
    let message = match failure {
        PosterFailureKind::Unavailable => "Poster unavailable",
        PosterFailureKind::Corrupt => "Poster cache was corrupt",
        PosterFailureKind::TimedOut => "Poster extraction timed out",
        PosterFailureKind::Io => "Poster I/O failed",
    };
    PosterStatus::Failed {
        message: message.to_owned(),
    }
}

fn publish_catalog_poster_status(
    results: CatalogResultSender,
    request: &PosterWorkRequest,
    status: PosterStatus,
) {
    retry_catalog_poster_result(
        results,
        CatalogResult::Poster {
            token: request.token.clone(),
            poster: PosterResult {
                path: request.token.path.clone(),
                status,
            },
        },
        ExpectedResultOwner::Poster(request.token.clone()),
    );
}

fn retry_catalog_poster_result(
    results: CatalogResultSender,
    result: CatalogResult,
    expected: ExpectedResultOwner,
) {
    match results.try_send_recoverable(result, expected) {
        Ok(_) => {}
        Err(rejected)
            if matches!(
                rejected.error,
                clipline_library::ResultPortError::Full { .. }
                    | clipline_library::ResultPortError::ByteCapacity { .. }
            ) =>
        {
            slint::Timer::single_shot(std::time::Duration::from_millis(2), move || {
                retry_catalog_poster_result(results, rejected.result, rejected.expected);
            });
        }
        Err(_) => {}
    }
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
